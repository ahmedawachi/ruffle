// Loads child.swf into the same clip ten times, then unloads it.
// Every load makes a new movie with its own library: the libraries of the
// movies that were replaced must be freed, while the library of the current
// movie must stay usable (`attachMovie` of a symbol exported by it).

var loads = 0;
var framesSinceUnload = -1;
var holder = this.createEmptyMovieClip("holder", 1);

function childLoaded(child) {
    loads++;
    var marker = child.attachMovie("__Packages.Marker", "marker", 1);
    trace("load " + loads + ": " + typeof(marker) + " " + marker._name);
    if (loads < 10) {
        holder.loadMovie("child.swf");
    } else {
        holder.unloadMovie();
        trace("unloaded");
        framesSinceUnload = 0;
    }
}

// Removed clips are let go of over the next frames: quit a few frames later.
this.onEnterFrame = function() {
    if (framesSinceUnload >= 0) {
        framesSinceUnload++;
        if (framesSinceUnload == 3) {
            delete this.onEnterFrame;
            fscommand("quit");
        }
    }
};

holder.loadMovie("child.swf");
