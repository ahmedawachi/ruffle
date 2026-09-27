use crate::avm1::{PropertyMap as Avm1PropertyMap, PropertyMap};
use crate::avm2::Domain as Avm2Domain;
use crate::backend::audio::SoundHandle;
use crate::character::Character;

use crate::display_object::{Bitmap, Graphic, MorphShape, Text};
use crate::font::{Font, FontDescriptor, FontLike, FontQuery, FontType};
use crate::prelude::*;
use crate::string::AvmString;
use crate::tag_utils::SwfMovie;
use core::fmt;
use gc_arena::barrier::unlock;
use gc_arena::collect::Trace;
use gc_arena::lock::RefLock;
use gc_arena::{Collect, Gc, GcWeak, Mutation};
use ruffle_render::backend::RenderBackend;
use ruffle_render::bitmap::BitmapHandle;
use ruffle_render::utils::remove_invalid_jpeg_data;
use ruffle_wstr::{WStr, WString};

use crate::backend::ui::{FontDefinition, UiBackend};
use crate::font::DefaultFont;
use fnv::{FnvHashMap, FnvHashSet};
use std::cell::{Cell, Ref, RefMut};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// Sounds that were registered with the audio backend by movie libraries that have
/// since been freed. Nothing can play them any more; the player releases them from
/// the audio backend after collecting garbage.
pub type ReleasedSounds = Rc<Cell<Vec<SoundHandle>>>;

/// The sounds that a movie library defined and registered with the audio backend.
///
/// They are handed over to be released when the library is freed. A library is only
/// freed once nothing can use it any more (see `MovieLibraryRef`), and everything that
/// can play one of its sounds keeps it alive: its display objects, the AVM1 `Sound`
/// objects that attached one of its sounds, the AVM2 classes linked to them, and the
/// libraries that imported them.
struct OwnedSounds {
    handles: Vec<SoundHandle>,
    released: ReleasedSounds,
}

impl Drop for OwnedSounds {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            let mut released = self.released.take();
            released.append(&mut self.handles);
            self.released.set(released);
        }
    }
}

/// A strong reference to the symbol library of a single movie.
///
/// Movie libraries are garbage collected: `Library` only keeps weak references
/// to them. The library of a movie, with its characters and the render and
/// memory resources they hold, lives exactly as long as something that can
/// still use it holds one of these: the display objects of the movie, the AVM2
/// code it defined, the classes linked to its symbols, the `LoaderInfo` of a
/// loaded movie and, for the root movie, the player.
///
/// A `MovieLibraryRef` only keeps the library alive. It is read and written
/// through `Library`, which ties every borrow of it to a borrow of `Library`,
/// so a library can never be borrowed mutably twice.
#[derive(Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct MovieLibraryRef<'gc>(Gc<'gc, MovieLibraryCell<'gc>>);

#[derive(Collect)]
#[collect(no_drop)]
struct MovieLibraryCell<'gc> {
    #[collect(require_static)]
    movie: Arc<SwfMovie>,
    library: RefLock<MovieLibrary<'gc>>,
}

impl fmt::Debug for MovieLibraryRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MovieLibraryRef")
            .field("ptr", &Gc::as_ptr(self.0))
            .field("url", &self.0.movie.url())
            .finish()
    }
}

impl<'gc> MovieLibraryRef<'gc> {
    fn new(movie: Arc<SwfMovie>, released_sounds: ReleasedSounds, mc: &Mutation<'gc>) -> Self {
        let this = Self(Gc::new(
            mc,
            MovieLibraryCell {
                movie: movie.clone(),
                library: RefLock::new(MovieLibrary::new(movie, released_sounds)),
            },
        ));
        // `Bitmap`s instantiated from the library hold it, so it needs to know itself.
        this.borrow_mut(mc).this = Some(this);
        this
    }

    /// The movie that this library belongs to.
    pub fn movie(self) -> Arc<SwfMovie> {
        self.0.movie.clone()
    }

    /// Returns true if both references point to the same library.
    pub fn ptr_eq(a: Self, b: Self) -> bool {
        Gc::ptr_eq(a.0, b.0)
    }

    fn borrow(self) -> Ref<'gc, MovieLibrary<'gc>> {
        Gc::as_ref(self.0).library.borrow()
    }

    fn borrow_mut(self, mc: &Mutation<'gc>) -> RefMut<'gc, MovieLibrary<'gc>> {
        unlock!(Gc::write(mc, self.0), MovieLibraryCell, library).borrow_mut()
    }
}

/// Symbol library for a single given SWF.
#[derive(Collect)]
#[collect(no_drop)]
pub struct MovieLibrary<'gc> {
    swf: Arc<SwfMovie>,
    characters: HashMap<CharacterId, Character<'gc>>,
    export_characters: Avm1PropertyMap<'gc, CharacterId>,
    imported_assets: HashMap<AvmString<'gc>, CharacterId>,
    jpeg_tables: Option<Vec<u8>>,
    fonts: FontMap<'gc>,
    avm2_domain: Option<Avm2Domain<'gc>>,
    /// The reference that owns this library; set as soon as it is created.
    this: Option<MovieLibraryRef<'gc>>,
    /// The sounds this library defined, released from the audio backend when it is freed.
    #[collect(require_static)]
    sounds: OwnedSounds,
    /// The libraries of other movies that own sounds this library shares (see
    /// `register_linked_character`), kept alive for as long as this one.
    linked_libraries: Vec<MovieLibraryRef<'gc>>,
}

impl<'gc> MovieLibrary<'gc> {
    fn new(swf: Arc<SwfMovie>, released_sounds: ReleasedSounds) -> Self {
        Self {
            swf,
            characters: HashMap::new(),
            imported_assets: HashMap::new(),
            export_characters: Avm1PropertyMap::new(),
            jpeg_tables: None,
            fonts: Default::default(),
            avm2_domain: None,
            this: None,
            sounds: OwnedSounds {
                handles: Vec::new(),
                released: released_sounds,
            },
            linked_libraries: Vec::new(),
        }
    }

    /// A strong reference to this library.
    fn library_ref(&self) -> MovieLibraryRef<'gc> {
        self.this
            .expect("a movie library is always owned by a MovieLibraryRef")
    }

    /// Registers a character; returns `true` if successful, or `false` if a character with
    /// the given ID already exists.
    pub fn register_character(&mut self, id: CharacterId, character: Character<'gc>) -> bool {
        use std::collections::hash_map::Entry;
        match self.characters.entry(id) {
            Entry::Vacant(e) => {
                if let Character::Font(font) = character {
                    self.fonts.register(font);
                }
                e.insert(character);
                true
            }
            Entry::Occupied(_) => {
                tracing::error!("Character ID collision: Tried to register ID {} twice", id);
                false
            }
        }
    }

    /// Registers a sound that this library's movie defined, which was registered with
    /// the audio backend as `handle`.
    ///
    /// The library owns the sound: it is released from the audio backend once the
    /// library is freed.
    pub fn register_sound(&mut self, id: CharacterId, handle: SoundHandle) -> bool {
        self.sounds.handles.push(handle);
        self.register_character(id, Character::Sound(handle))
    }

    /// Registers a character of another movie's library (shared by `ImportAssets`).
    ///
    /// A sound belongs to the library that defined it, so that library is kept alive
    /// for as long as this one: this library can still play the sound. The other kinds
    /// of characters don't need that: they either keep their library alive themselves
    /// or don't depend on it.
    pub fn register_linked_character(
        &mut self,
        id: CharacterId,
        character: Character<'gc>,
        from: MovieLibraryRef<'gc>,
    ) -> bool {
        if matches!(character, Character::Sound(_)) {
            self.link_library(from);
        }
        self.register_character(id, character)
    }

    fn link_library(&mut self, library: MovieLibraryRef<'gc>) {
        let is_self = self
            .this
            .is_some_and(|this| MovieLibraryRef::ptr_eq(this, library));
        let is_linked = self
            .linked_libraries
            .iter()
            .any(|&linked| MovieLibraryRef::ptr_eq(linked, library));
        if !is_self && !is_linked {
            self.linked_libraries.push(library);
        }
    }

    /// Registers an export name for a given character ID.
    /// This character will then be instantiable from AVM1.
    pub fn register_export(&mut self, id: CharacterId, export_name: AvmString<'gc>) {
        let character_exists = self.contains_character(id);
        debug_assert!(character_exists);
        if !character_exists {
            tracing::error!(
                "Tried to register export '{export_name}' for a non-existent character {id}"
            );
            return;
        }

        self.export_characters.insert(export_name, id, false);
    }

    pub fn register_font_name(&mut self, character_id: u16, font_name: &str) {
        let Some(Character::Font(font)) = self.character_by_id(character_id) else {
            return;
        };

        let descriptor = FontDescriptor::from_parts(
            font_name,
            font.descriptor().bold(),
            font.descriptor().italic(),
        );
        self.fonts.register_with_descriptor(font, &descriptor);
    }

    pub fn characters(&self) -> &HashMap<CharacterId, Character<'gc>> {
        &self.characters
    }

    pub fn export_characters(&self) -> &PropertyMap<'gc, CharacterId> {
        &self.export_characters
    }

    pub fn contains_character(&self, id: CharacterId) -> bool {
        self.characters.contains_key(&id)
    }

    pub fn character_by_id(&self, id: CharacterId) -> Option<Character<'gc>> {
        self.characters.get(&id).copied()
    }

    pub fn character_by_export_name(&self, name: &WStr) -> Option<(CharacterId, Character<'gc>)> {
        if let Some(id) = self.export_characters.get(name, false)
            && let Some(character) = self.characters.get(id)
        {
            return Some((*id, *character));
        }
        None
    }

    pub fn character_id_by_import_name(&self, name: &WStr) -> Option<CharacterId> {
        self.imported_assets.get(name).copied()
    }

    pub fn register_import(&mut self, name: AvmString<'gc>, id: CharacterId) {
        self.imported_assets.insert(name, id);
    }

    /// Instantiates the library item with the given character ID into a display object.
    /// The object must then be post-instantiated before being used.
    pub fn instantiate_by_id(
        &self,
        id: CharacterId,
        mc: &Mutation<'gc>,
    ) -> Option<DisplayObject<'gc>> {
        if let Some(&character) = self.characters.get(&id) {
            self.instantiate_display_object(id, character, mc)
        } else {
            tracing::error!("Tried to instantiate a non-registered character ID {id}");
            None
        }
    }

    /// Instantiates the library item with the given export name into a display object.
    /// The object must then be post-instantiated before being used.
    pub fn instantiate_by_export_name(
        &self,
        export_name: &WStr,
        mc: &Mutation<'gc>,
    ) -> Option<DisplayObject<'gc>> {
        if let Some((id, character)) = self.character_by_export_name(export_name) {
            self.instantiate_display_object(id, character, mc)
        } else {
            tracing::error!("Tried to instantiate a non-registered character {export_name}");
            None
        }
    }

    /// Instantiates the given character into a display object.
    /// The object must then be post-instantiated before being used.
    fn instantiate_display_object(
        &self,
        id: CharacterId,
        character: Character<'gc>,
        mc: &Mutation<'gc>,
    ) -> Option<DisplayObject<'gc>> {
        match character {
            Character::Bitmap(bitmap) => {
                let avm2_class = bitmap.avm2_class();
                let bitmap = bitmap.compressed().decode().unwrap();
                let bitmap = Bitmap::new(mc, id, bitmap, self.library_ref());
                bitmap.set_avm2_bitmapdata_class(mc, avm2_class);
                Some(bitmap.instantiate(mc).into())
            }
            Character::EditText(edit_text) => Some(edit_text.instantiate(mc).into()),
            Character::Graphic(graphic) => Some(graphic.instantiate(mc).into()),
            Character::MorphShape(morph_shape) => Some(morph_shape.instantiate(mc).into()),
            Character::MovieClip(movie_clip) => Some(movie_clip.instantiate(mc).into()),
            Character::Avm1Button(button) => Some(button.instantiate(mc).into()),
            Character::Avm2Button(button) => Some(button.instantiate(mc).into()),
            Character::Text(text) => Some(text.instantiate(mc).into()),
            Character::Video(video) => Some(video.instantiate(mc).into()),
            _ => {
                // Cannot instantiate non-display object
                None
            }
        }
    }

    pub fn get_font(&self, id: CharacterId) -> Option<Font<'gc>> {
        if let Some(&Character::Font(font)) = self.characters.get(&id) {
            Some(font)
        } else {
            None
        }
    }

    pub fn embedded_fonts(&self) -> Vec<Font<'gc>> {
        self.fonts.all()
    }

    /// Returns the `Graphic` with the given character ID.
    /// Returns `None` if the ID does not exist or is not a `Graphic`.
    pub fn get_graphic(&self, id: CharacterId) -> Option<Graphic<'gc>> {
        if let Some(&Character::Graphic(graphic)) = self.characters.get(&id) {
            Some(graphic)
        } else {
            None
        }
    }

    /// Returns the `MorphShape` with the given character ID.
    /// Returns `None` if the ID does not exist or is not a `MorphShape`.
    pub fn get_morph_shape(&self, id: CharacterId) -> Option<MorphShape<'gc>> {
        if let Some(&Character::MorphShape(morph_shape)) = self.characters.get(&id) {
            Some(morph_shape)
        } else {
            None
        }
    }

    pub fn get_sound(&self, id: CharacterId) -> Option<SoundHandle> {
        if let Some(Character::Sound(sound)) = self.characters.get(&id) {
            Some(*sound)
        } else {
            None
        }
    }

    /// Returns the `Text` with the given character ID.
    /// Returns `None` if the ID does not exist or is not a `Text`.
    pub fn get_text(&self, id: CharacterId) -> Option<Text<'gc>> {
        if let Some(&Character::Text(text)) = self.characters.get(&id) {
            Some(text)
        } else {
            None
        }
    }

    pub fn set_jpeg_tables(&mut self, data: &[u8]) {
        if self.jpeg_tables.is_some() {
            // SWF spec says there should only be one JPEGTables tag.
            // TODO: What is the behavior when there are multiples?
            tracing::warn!("SWF contains multiple JPEGTables tags");
            return;
        }
        // Some SWFs have a JPEGTables tag with 0 length; ignore these.
        // (Does this happen when there is only a single DefineBits tag?)
        self.jpeg_tables = if data.is_empty() {
            None
        } else {
            Some(remove_invalid_jpeg_data(data).to_vec())
        }
    }

    pub fn jpeg_tables(&self) -> Option<&[u8]> {
        self.jpeg_tables.as_ref().map(|data| &data[..])
    }

    pub fn set_avm2_domain(&mut self, avm2_domain: Avm2Domain<'gc>) {
        self.avm2_domain = Some(avm2_domain);
    }

    /// Get the AVM2 domain this movie runs under.
    ///
    /// Note that the presence of an AVM2 domain does *not* indicate that this
    /// movie provides AVM2 code. For example, a movie may have been loaded by
    /// AVM2 code into a particular domain, even though it turned out to be
    /// an AVM1 movie, and thus this domain is unused.
    pub fn avm2_domain(&self) -> Avm2Domain<'gc> {
        self.avm2_domain.unwrap()
    }

    pub fn try_avm2_domain(&self) -> Option<Avm2Domain<'gc>> {
        self.avm2_domain
    }
}

pub struct MovieLibrarySource<'a, 'gc> {
    pub library: &'a MovieLibrary<'gc>,
}

impl ruffle_render::bitmap::BitmapSource for MovieLibrarySource<'_, '_> {
    fn bitmap_size(&self, id: u16) -> Option<ruffle_render::bitmap::BitmapSize> {
        if let Some(Character::Bitmap(bitmap)) = self.library.characters.get(&id) {
            Some(bitmap.compressed().size())
        } else {
            None
        }
    }

    fn bitmap_handle(&self, id: u16, backend: &mut dyn RenderBackend) -> Option<BitmapHandle> {
        let Some(Character::Bitmap(bitmap)) = self.library.characters.get(&id) else {
            return None;
        };

        match bitmap.bitmap_handle(backend) {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::error!("Failed to register bitmap character {id}: {e}");
                None
            }
        }
    }
}

/// Weak references to the libraries of all movies, keyed by the address of their movie.
///
/// A library holds its movie until it is dropped, so while an entry has not been
/// dropped, no other movie can have its address. Entries of dropped libraries are
/// pruned before inserting.
struct MovieLibraries<'gc> {
    libraries: FnvHashMap<*const SwfMovie, GcWeak<'gc, MovieLibraryCell<'gc>>>,
    /// Where freed libraries hand over the sounds they registered.
    released_sounds: ReleasedSounds,
}

unsafe impl<'gc> Collect<'gc> for MovieLibraries<'gc> {
    #[inline]
    fn trace<C: Trace<'gc>>(&self, cc: &mut C) {
        for library in self.libraries.values() {
            cc.trace(library);
        }
    }
}

impl<'gc> MovieLibraries<'gc> {
    fn new(released_sounds: ReleasedSounds) -> Self {
        Self {
            libraries: FnvHashMap::default(),
            released_sounds,
        }
    }

    fn get(&self, movie: &Arc<SwfMovie>, mc: &Mutation<'gc>) -> Option<MovieLibraryRef<'gc>> {
        let library = MovieLibraryRef(self.libraries.get(&Arc::as_ptr(movie))?.upgrade(mc)?);
        debug_assert!(Arc::ptr_eq(&library.0.movie, movie));
        Some(library)
    }

    fn get_or_insert(&mut self, movie: Arc<SwfMovie>, mc: &Mutation<'gc>) -> MovieLibraryRef<'gc> {
        if let Some(library) = self.get(&movie, mc) {
            return library;
        }
        self.libraries.retain(|_, library| !library.is_dropped());
        let library = MovieLibraryRef::new(movie, self.released_sounds.clone(), mc);
        self.libraries
            .insert(Arc::as_ptr(&library.0.movie), Gc::downgrade(library.0));
        library
    }

    fn live(&self, mc: &Mutation<'gc>) -> impl Iterator<Item = MovieLibraryRef<'gc>> {
        self.libraries
            .values()
            .filter_map(move |library| library.upgrade(mc).map(MovieLibraryRef))
    }
}

/// Symbol library for multiple movies.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Library<'gc> {
    /// All the movie libraries.
    movie_libraries: MovieLibraries<'gc>,

    /// The library of the root movie, which is looked up through
    /// `UpdateContext::root_swf` even when no display object of the root movie is left.
    root_library: Option<MovieLibraryRef<'gc>>,

    /// A cache of seen device fonts.
    // TODO: Descriptors shouldn't be stored in fonts. Fonts should be a list that we iterate and ask "do you match". A font can have zero or many names.
    device_fonts: FontMap<'gc>,

    /// "Global" embedded fonts, registered through AVM2 `Font.registerFont`.
    /// These should be checked before any Movie-specific library's own fonts.
    global_fonts: FontMap<'gc>,

    /// A set of which fonts we've asked from the backend already, to help with negative caching.
    /// If we've asked for a specific font, record it here and don't ask again.
    font_lookup_cache: FnvHashSet<FontQuery>,

    /// Cached font sort queries.
    font_sort_cache: FnvHashMap<FontQuery, Vec<Font<'gc>>>,

    /// The implementation names of each default font.
    default_font_names: FnvHashMap<DefaultFont, Vec<String>>,

    /// The cached list of implementations per default font.
    default_font_cache: FnvHashMap<(DefaultFont, bool, bool), Vec<Font<'gc>>>,
}

impl<'gc> Library<'gc> {
    /// Creates an empty library. Freed movie libraries hand the sounds they registered
    /// with the audio backend over to `released_sounds`.
    pub fn empty(released_sounds: ReleasedSounds) -> Self {
        Self {
            movie_libraries: MovieLibraries::new(released_sounds),
            root_library: None,
            device_fonts: Default::default(),
            global_fonts: Default::default(),
            font_lookup_cache: Default::default(),
            font_sort_cache: Default::default(),
            default_font_names: Default::default(),
            default_font_cache: Default::default(),
        }
    }

    /// Returns the library of the given movie, if it has one.
    ///
    /// A library is only found while something holds a `MovieLibraryRef` to it (see
    /// `library_ref`): everything that uses the library of a movie must keep it alive.
    pub fn library_for_movie(
        &self,
        movie: Arc<SwfMovie>,
        mc: &Mutation<'gc>,
    ) -> Option<Ref<'_, MovieLibrary<'gc>>> {
        self.movie_libraries
            .get(&movie, mc)
            .map(MovieLibraryRef::borrow)
    }

    /// Returns the library of the given movie, creating an empty one if it has none.
    pub fn library_for_movie_mut(
        &mut self,
        movie: Arc<SwfMovie>,
        mc: &Mutation<'gc>,
    ) -> RefMut<'_, MovieLibrary<'gc>> {
        self.movie_libraries.get_or_insert(movie, mc).borrow_mut(mc)
    }

    /// Returns a strong reference to the library of the given movie, creating
    /// an empty one if it has none.
    ///
    /// Everything that can use the library of a movie later on (display objects,
    /// AVM2 code, class links, loader infos) holds one of these, which keeps the
    /// library alive.
    pub fn library_ref(
        &mut self,
        movie: Arc<SwfMovie>,
        mc: &Mutation<'gc>,
    ) -> MovieLibraryRef<'gc> {
        self.movie_libraries.get_or_insert(movie, mc)
    }

    /// Borrows a library that is kept alive by the given reference.
    pub fn get(&self, library: MovieLibraryRef<'gc>) -> Ref<'_, MovieLibrary<'gc>> {
        library.borrow()
    }

    /// Mutably borrows a library that is kept alive by the given reference.
    pub fn get_mut(
        &mut self,
        library: MovieLibraryRef<'gc>,
        mc: &Mutation<'gc>,
    ) -> RefMut<'_, MovieLibrary<'gc>> {
        library.borrow_mut(mc)
    }

    /// Makes the library of the given movie live as long as it is the root movie:
    /// the root movie is looked up through `UpdateContext::root_swf` even when none
    /// of its display objects are left.
    pub fn set_root_movie(&mut self, movie: Arc<SwfMovie>, mc: &Mutation<'gc>) {
        self.root_library = Some(self.library_ref(movie, mc));
    }

    /// The movies that currently have a library.
    pub fn known_movies(&self, mc: &Mutation<'gc>) -> Vec<Arc<SwfMovie>> {
        self.movie_libraries
            .live(mc)
            .map(MovieLibraryRef::movie)
            .collect()
    }

    /// Returns the default Font implementations behind the built in names (ie `_sans`)
    pub fn default_font(
        &mut self,
        name: DefaultFont,
        is_bold: bool,
        is_italic: bool,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Vec<Font<'gc>> {
        // Can't use entry api here as we want to use self for `load_device_font`.
        // Cache the value as this will be looked up a lot, and font lookup by name can be expensive if lots of fonts exist.
        if let Some(cache) = self.default_font_cache.get(&(name, is_bold, is_italic)) {
            return cache.clone();
        }

        let mut result = vec![];
        // First try to find any exactly matching fonts.
        for name in self.default_font_names.entry(name).or_default().clone() {
            let query = FontQuery::new(FontType::Device, name, is_bold, is_italic);
            if let Some(font) = self.get_or_load_exact_device_font(&query, ui, renderer, gc_context)
            {
                result.push(font);
                break; // TODO: Return multiple fonts when it's needed.
            }
        }

        // Nothing found, try a compatible font.
        if result.is_empty() {
            for name in self.default_font_names.entry(name).or_default().clone() {
                let query = FontQuery::new(FontType::Device, name, is_bold, is_italic);
                if let Some(font) = self.device_fonts.find(&query) {
                    result.push(font);
                    break; // TODO: Return multiple fonts when it's needed.
                }
            }
        }

        self.default_font_cache
            .insert((name, is_bold, is_italic), result.clone());
        result
    }

    /// Returns the device font exactly matching the requested options.
    fn get_or_load_exact_device_font(
        &mut self,
        query: &FontQuery,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Option<Font<'gc>> {
        // If we have the exact matching font already, use that
        // TODO: We should instead ask each font if it matches a given name. Partial matches are allowed, and fonts may have any amount of names.
        if let Some(font) = self.device_fonts.get(query) {
            return Some(font);
        }

        // We don't have this font already. Did we ask for it before?
        let new_request = self.font_lookup_cache.insert(query.clone());
        if new_request {
            // First time asking for this font, see if our backend can provide anything relevant
            ui.load_device_font(query, &mut |definition| {
                self.register_device_font(gc_context, renderer, definition)
            });

            // Check again. A backend may or may not have provided some new fonts,
            // and they may or may not be relevant to the one we're asking for.
            if let Some(font) = self.device_fonts.get(query) {
                return Some(font);
            }

            let name = &query.name;
            let is_bold = query.is_bold;
            let is_italic = query.is_italic;
            warn!("Unknown device font \"{name}\" (bold: {is_bold}, italic: {is_italic})");
        }

        None
    }

    /// Returns the device font compatible with the requested options.
    pub fn get_or_load_device_font(
        &mut self,
        name: &str,
        is_bold: bool,
        is_italic: bool,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Option<Font<'gc>> {
        let query = FontQuery::new(FontType::Device, name.to_owned(), is_bold, is_italic);

        // Try to find an exactly matching font.
        if let Some(font) = self.get_or_load_exact_device_font(&query, ui, renderer, gc_context) {
            return Some(font);
        }

        // Fallback: Try to find an existing font to re-use instead of giving up.
        self.device_fonts.find(&query)
    }

    fn sort_device_fonts(
        &mut self,
        query: &FontQuery,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Vec<Font<'gc>> {
        // First, ask the backend to sort the fonts for us.
        let fonts = ui.sort_device_fonts(query, &mut |definition| {
            self.register_device_font(gc_context, renderer, definition)
        });

        let fonts: Vec<Font<'gc>> = fonts
            .iter()
            .filter_map(|font_query| self.device_fonts.get(font_query))
            .collect();

        if !fonts.is_empty() {
            return fonts;
        }

        // When the backend failed (or doesn't support sorting fonts), fall back
        // to loading one font only without sorting.
        let font = self.get_or_load_device_font(
            &query.name,
            query.is_bold,
            query.is_italic,
            ui,
            renderer,
            gc_context,
        );
        font.map(|font| vec![font]).unwrap_or_default()
    }

    pub fn get_or_sort_device_fonts(
        &mut self,
        name: &str,
        is_bold: bool,
        is_italic: bool,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Vec<Font<'gc>> {
        // TODO We should be able to return a &Vec here, but (1) the borrow
        //   checker is too strict and doesn't allow if branching, and
        //   (2) there's no way to insert a value and get a reference to
        //   it at the same time.
        let query = FontQuery::new(FontType::Device, name.to_owned(), is_bold, is_italic);
        if let Some(fonts) = self.font_sort_cache.get(&query) {
            return fonts.clone();
        }

        let fonts = self.sort_device_fonts(&query, ui, renderer, gc_context);
        self.font_sort_cache.insert(query, fonts.clone());
        fonts
    }

    pub fn set_default_font(&mut self, font: DefaultFont, names: Vec<String>) {
        self.default_font_names.insert(font, names);
        self.default_font_cache.clear();
    }

    pub fn register_device_font(
        &mut self,
        gc_context: &Mutation<'gc>,
        renderer: &mut dyn RenderBackend,
        definition: FontDefinition<'_>,
    ) {
        match definition {
            FontDefinition::SwfTag(tag, encoding) => {
                let font =
                    Font::from_swf_tag(gc_context, renderer, tag, encoding, FontType::Device);
                let name = font.descriptor().name().to_owned();
                let is_bold = font.descriptor().bold();
                let is_italic = font.descriptor().italic();
                tracing::debug!(
                    "Loaded new device font \"{name}\" (bold: {is_bold}, italic: {is_italic}) from swf tag"
                );
                self.device_fonts.register(font);
            }
            FontDefinition::FontFile {
                name,
                is_bold,
                is_italic,
                data,
                index,
            } => {
                let descriptor = FontDescriptor::from_parts(&name, is_bold, is_italic);
                if let Ok(font) =
                    Font::from_font_file(gc_context, descriptor, data, index, FontType::Device)
                {
                    let name = font.descriptor().name().to_owned();
                    tracing::debug!(
                        "Loaded new device font \"{name}\" (bold: {is_bold}, italic: {is_italic}) from file"
                    );
                    self.device_fonts.register(font);
                } else {
                    warn!("Failed to load device font from file");
                }
            }
            FontDefinition::ExternalRenderer {
                name,
                is_bold,
                is_italic,
                font_renderer,
            } => {
                let descriptor = FontDescriptor::from_parts(&name, is_bold, is_italic);
                let font = Font::from_renderer(gc_context, descriptor, font_renderer);
                tracing::debug!(
                    "Loaded new externally rendered font \"{name}\" (bold: {is_bold}, italic: {is_italic})"
                );
                self.device_fonts.register(font);
            }
        }
        self.default_font_cache.clear();
    }

    /// Find a font by its name and parameters.
    pub fn get_embedded_font_by_name(
        &self,
        name: &str,
        font_type: FontType,
        is_bold: bool,
        is_italic: bool,
        movie: Option<Arc<SwfMovie>>,
        mc: &Mutation<'gc>,
    ) -> Option<Font<'gc>> {
        let query = FontQuery::new(font_type, name.to_owned(), is_bold, is_italic);
        if let Some(font) = self.global_fonts.find(&query) {
            return Some(font);
        }
        if let Some(movie) = movie
            && let Some(library) = self.library_for_movie(movie, mc)
        {
            if let Some((_, font)) = library.character_by_export_name(&WString::from_utf8(name)) {
                // Exporting a font seems to override font lookup completely.
                return if let Character::Font(font) = font {
                    Some(font)
                } else {
                    None
                };
            }
            if let Some(font) = library.fonts.find(&query) {
                return Some(font);
            }
        }
        None
    }

    pub fn global_fonts(&self) -> Vec<Font<'gc>> {
        self.global_fonts.all()
    }

    pub fn register_global_font(&mut self, font: Font<'gc>) {
        self.global_fonts.register(font);
    }

    /// Evicts cached font resources that haven't been used, across all device
    /// fonts. Meant to be called once per rendered frame.
    pub fn sweep_font_caches(&self) {
        for font in self.device_fonts.iter_all() {
            font.sweep_caches();
        }
    }
}

#[derive(Collect, Default)]
#[collect(no_drop)]
struct FontMap<'gc>(FnvHashMap<FontQuery, Font<'gc>>);

impl<'gc> FontMap<'gc> {
    pub fn register(&mut self, font: Font<'gc>) {
        self.register_with_descriptor(font, font.descriptor());
    }

    pub fn register_with_descriptor(&mut self, font: Font<'gc>, descriptor: &FontDescriptor) {
        self.0
            .entry(FontQuery::from_descriptor(font.font_type(), descriptor))
            .or_insert(font);
    }

    pub fn get(&self, font_query: &FontQuery) -> Option<Font<'gc>> {
        self.0.get(font_query).copied()
    }

    pub fn find(&self, font_query: &FontQuery) -> Option<Font<'gc>> {
        // The order here is specific, and tested in `tests/swfs/fonts/embed_matching/fallback_preferences`

        // Exact match
        if let Some(font) = self.get(font_query) {
            return Some(font);
        }

        let is_italic = font_query.is_italic;
        let is_bold = font_query.is_bold;

        let mut fallback_query = font_query.clone();
        if is_italic ^ is_bold {
            // If one is set (but not both), then try upgrading to bold italic...
            fallback_query.is_bold = true;
            fallback_query.is_italic = true;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            // and then downgrading to regular
            fallback_query.is_bold = false;
            fallback_query.is_italic = false;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            // and then finally whichever one we don't have set
            fallback_query.is_bold = !is_bold;
            fallback_query.is_italic = !is_italic;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }
        } else {
            // We don't have an exact match and we were either looking for regular or bold-italic

            if is_italic && is_bold {
                // Do we have regular? (unless we already looked for it)
                fallback_query.is_bold = false;
                fallback_query.is_italic = false;
                if let Some(font) = self.get(&fallback_query) {
                    return Some(font);
                }
            }

            // Do we have bold?
            fallback_query.is_bold = true;
            fallback_query.is_italic = false;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            // Do we have italic?
            fallback_query.is_bold = false;
            fallback_query.is_italic = true;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            if !is_bold && !is_italic {
                // Do we have bold italic? (unless we already looked for it)
                fallback_query.is_bold = true;
                fallback_query.is_italic = true;
                if let Some(font) = self.get(&fallback_query) {
                    return Some(font);
                }
            }
        }

        None
    }

    pub fn all(&self) -> Vec<Font<'gc>> {
        self.0.values().copied().collect()
    }

    pub fn iter_all(&self) -> impl Iterator<Item = Font<'gc>> {
        self.0.values().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::UpdateContext;
    use crate::player::PlayerBuilder;

    /// Registers a sound defined by `library`'s movie, as a `DefineSound` tag does.
    fn define_sound<'gc>(
        context: &mut UpdateContext<'gc>,
        library: MovieLibraryRef<'gc>,
        id: CharacterId,
    ) -> SoundHandle {
        let data = [0u8; 16];
        let handle = context
            .audio
            .register_sound(&swf::Sound {
                id,
                format: swf::SoundFormat {
                    compression: swf::AudioCompression::Uncompressed,
                    sample_rate: 44100,
                    is_stereo: false,
                    is_16_bit: false,
                },
                num_samples: 16,
                data: &data,
            })
            .expect("the sound should register");
        context
            .library
            .get_mut(library, context.gc_context)
            .register_sound(id, handle);
        handle
    }

    #[test]
    fn sounds_are_released_with_their_library() {
        let player = PlayerBuilder::new().build();
        let mut player = player.lock().unwrap();
        let root = Arc::new(SwfMovie::empty(8, None));
        let other = Arc::new(SwfMovie::empty(8, None));

        let (kept, released) = player.mutate_with_update_context(|context| {
            context
                .library
                .set_root_movie(root.clone(), context.gc_context);
            let root_library = context.library.library_ref(root, context.gc_context);
            let other_library = context.library.library_ref(other, context.gc_context);
            (
                define_sound(context, root_library, 1),
                define_sound(context, other_library, 1),
            )
        });
        player.collect_garbage();

        // The player keeps the library of the root movie alive; nothing keeps the other one.
        assert!(player.audio().get_sound_duration(kept).is_some());
        assert!(player.audio().get_sound_duration(released).is_none());
    }

    #[test]
    fn imported_sounds_keep_their_library_alive() {
        let player = PlayerBuilder::new().build();
        let mut player = player.lock().unwrap();
        let importer = Arc::new(SwfMovie::empty(8, None));
        let exporter = Arc::new(SwfMovie::empty(8, None));

        let sound = player.mutate_with_update_context(|context| {
            context
                .library
                .set_root_movie(importer.clone(), context.gc_context);
            let importer_library = context.library.library_ref(importer, context.gc_context);
            let exporter_library = context.library.library_ref(exporter, context.gc_context);
            let sound = define_sound(context, exporter_library, 1);
            context
                .library
                .get_mut(importer_library, context.gc_context)
                .register_linked_character(2, Character::Sound(sound), exporter_library);
            sound
        });
        player.collect_garbage();

        // Only the importer's library is held, and it can still play the exporter's sound.
        assert!(player.audio().get_sound_duration(sound).is_some());

        let replacement = Arc::new(SwfMovie::empty(8, None));
        player.mutate_with_update_context(|context| {
            context
                .library
                .set_root_movie(replacement, context.gc_context);
        });
        player.collect_garbage();
        assert!(player.audio().get_sound_duration(sound).is_none());
    }
}
