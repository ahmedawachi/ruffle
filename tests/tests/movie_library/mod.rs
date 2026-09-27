use ruffle_test_framework::environment::Environment;
use ruffle_test_framework::options::TestOptions;
use ruffle_test_framework::runner::TestStatus;
use ruffle_test_framework::test::Test;
use ruffle_test_framework::vfs::{PhysicalFS, VfsPath};
use std::thread::sleep;

pub fn movie_library_lifetime_avm1(
    environment: &impl Environment,
) -> Result<(), libtest_mimic::Failed> {
    // Load a movie into the same clip over and over, collecting garbage after
    // every tick: the libraries of the replaced movies must be freed, and the
    // library of the current movie must stay usable (the SWF checks this).
    let root_path = VfsPath::new(PhysicalFS::new("tests/swfs/avm1/movie_library_lifetime/"));
    let options = TestOptions::read(&root_path.join("options.toml")?)?;
    let test = Test::from_options(
        options,
        root_path,
        "movie_library_lifetime_avm1".to_string(),
    )?;
    let mut runner = test.create_test_runner(environment)?;

    loop {
        let status = runner.tick()?;
        runner.player().lock().unwrap().collect_garbage();
        match status {
            TestStatus::Continue => {}
            TestStatus::Sleep(duration) => sleep(duration),
            TestStatus::Finished => break,
        }
    }

    let mut player = runner.player().lock().unwrap();
    player.collect_garbage();
    let child_libraries = player.mutate_with_update_context(|context| {
        context
            .library
            .known_movies(context.gc_context)
            .iter()
            .filter(|movie| movie.url().ends_with("child.swf"))
            .count()
    });
    assert_eq!(
        child_libraries, 0,
        "the libraries of movies that were unloaded should be freed"
    );

    Ok(())
}
