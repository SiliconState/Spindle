//! Builtin scheduling and validation metadata, not execution authority.
#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    required_fields: &'static [&'static str],
    flags: u8,
}

const PERMISSION_REQUIRED: u8 = 1;
const PARALLEL_SAFE: u8 = 1 << 1;
const EXTERNAL_PROCESS: u8 = 1 << 2;
const DEFAULT_PROFILE: u8 = 1 << 3;
const SENSITIVE_READ: u8 = 1 << 4;
const REPLAY_SAFE: u8 = 1 << 5;

const fn tool(name: &'static str, required_fields: &'static [&'static str], flags: u8) -> ToolSpec {
    ToolSpec {
        name,
        required_fields,
        flags,
    }
}

const TOOL_SPECS: &[ToolSpec] = &[
    tool(
        "read_file",
        &["path"],
        PARALLEL_SAFE | REPLAY_SAFE | DEFAULT_PROFILE,
    ),
    tool(
        "read_image",
        &["path"],
        PERMISSION_REQUIRED | SENSITIVE_READ | DEFAULT_PROFILE,
    ),
    tool(
        "read_symbol",
        &["path"],
        PARALLEL_SAFE | REPLAY_SAFE | DEFAULT_PROFILE,
    ),
    tool(
        "write_file",
        &["path", "content"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "edit_file",
        &["path", "old_string", "new_string"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "multi_edit",
        &["path", "edits"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool("bash", &["command"], PERMISSION_REQUIRED | DEFAULT_PROFILE),
    tool(
        "fd",
        &["pattern"],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool(
        "rg",
        &["pattern"],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool(
        "jq",
        &["filter"],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS,
    ),
    tool(
        "fzf",
        &["query", "items"],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS,
    ),
    tool("http", &["args"], PERMISSION_REQUIRED | DEFAULT_PROFILE),
    tool("awk", &["args"], PERMISSION_REQUIRED | EXTERNAL_PROCESS),
    tool(
        "git_diff",
        &[],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool(
        "git_status",
        &[],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool(
        "git_log",
        &[],
        PARALLEL_SAFE | REPLAY_SAFE | EXTERNAL_PROCESS,
    ),
    tool(
        "git_commit",
        &["message"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "todo_read",
        &[],
        PARALLEL_SAFE | REPLAY_SAFE | DEFAULT_PROFILE,
    ),
    tool(
        "todo_write",
        &["todos"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "csvkit",
        &["subcommand", "args"],
        PERMISSION_REQUIRED | EXTERNAL_PROCESS,
    ),
];

fn tool_spec(name: &str) -> Option<&'static ToolSpec> {
    TOOL_SPECS.iter().find(|spec| spec.name == name)
}

pub fn required_fields(name: &str) -> &'static [&'static str] {
    tool_spec(name).map_or(&[], |spec| spec.required_fields)
}
pub fn is_default_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & DEFAULT_PROFILE != 0)
}
pub fn registered_tool_names() -> impl Iterator<Item = &'static str> {
    TOOL_SPECS.iter().map(|spec| spec.name)
}
pub fn specialized_tool_names() -> impl Iterator<Item = &'static str> {
    TOOL_SPECS
        .iter()
        .filter(|spec| spec.flags & DEFAULT_PROFILE == 0)
        .map(|spec| spec.name)
}
pub fn needs_permission(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & PERMISSION_REQUIRED != 0)
}
pub fn is_sensitive_read_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & SENSITIVE_READ != 0)
}
pub fn is_side_effect_capable_tool(name: &str) -> bool {
    needs_permission(name) && !is_sensitive_read_tool(name)
}
pub fn is_external_process_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & EXTERNAL_PROCESS != 0)
}
pub fn is_replay_safe_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & REPLAY_SAFE != 0)
}
pub fn is_parallel_safe_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & PARALLEL_SAFE != 0)
}
pub fn should_parallelize_builtin_tools(names: &[&str]) -> bool {
    !names.is_empty() && names.iter().all(|name| is_parallel_safe_tool(name))
}
