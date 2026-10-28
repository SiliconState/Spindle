fn main() {
    if std::env::args_os().nth(1).as_deref()
        != Some(std::ffi::OsStr::new(dext_native::image::WORKER_ARG))
    {
        std::process::exit(2);
    }
    std::process::exit(dext_native::image::worker_main());
}
