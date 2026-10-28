// The agent is one process for every command and holds what their scripts send until
// Spindle takes it (agent.rs: MAX_HELD): it may have up to 1 GiB of memory, the other
// programs the toolchain's 256 MiB (build-toolchain.sh). Memory a process does not use
// costs nothing: the most is only reserved.
fn main() {
    println!("cargo:rustc-link-arg-bin=spindle-agent=-Wl,--max-memory=1073741824");
}
