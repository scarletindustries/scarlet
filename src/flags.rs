#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub expose_debug_builtins: bool,
    pub io_enabled: bool,
    pub std_lib_enabled: bool,
}
