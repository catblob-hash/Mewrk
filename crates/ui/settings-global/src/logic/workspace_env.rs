//! The pure parts of `WorkspaceSettings.tsx` and `MachineDialogs.tsx`: what the host will refuse, refused at the field.

use std::collections::BTreeMap;

use mewrk_client::{t, tf};

pub const MAX_ENV_VARS_PER_TABLE: usize = 128;
pub const MAX_ENV_VALUE_CHARS: usize = 8192;
pub const MAX_SSH_MACHINES: usize = 64;
pub const MAX_MACHINE_NAME_CHARS: usize = 64;
pub const MAX_HOST_CHARS: usize = 512;
pub const MAX_PATH_FIELD_CHARS: usize = 4096;

fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| (c as u32) <= 0x1F || c as u32 == 0x7F)
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Host-reserved shell-startup and private child-environment names.
pub fn reserved_env_var_name(name: &str) -> bool {
    if ["BASH_ENV", "ENV", "SHELLOPTS", "BASHOPTS", "CDPATH", "GLOBIGNORE", "GIT_EXTERNAL_DIFF"].contains(&name) {
        return true;
    }
    let upper = name.to_uppercase();
    (upper.starts_with("MEWRK_") || upper.starts_with("VITE_")) && (upper.contains("BROWSER_DEV") || upper.contains("E2E"))
}

pub fn format_env_text(vars: &BTreeMap<String, String>) -> String {
    vars.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n")
}

#[derive(Default, Debug)]
pub struct ParsedEnvText {
    pub vars: BTreeMap<String, String>,
    pub invalid: Vec<String>,
    pub reserved: Vec<String>,
    pub bad_values: Vec<String>,
    pub too_many: bool,
}

/// `KEY=value` lines. Everything after the first `=` is kept verbatim; only keys are trimmed.
pub fn parse_env_text(text: &str) -> ParsedEnvText {
    let mut out = ParsedEnvText::default();
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if line.trim().is_empty() {
            continue;
        }
        let position = line.find('=');
        let key = position.map_or_else(|| line.trim(), |p| line[..p].trim());
        if key.is_empty() {
            continue;
        }
        if position.is_none() || !is_env_name(key) || key.chars().count() > 128 {
            out.invalid.push(key.to_owned());
            continue;
        }
        if reserved_env_var_name(key) {
            out.reserved.push(key.to_owned());
            continue;
        }
        let value = &line[position.expect("checked") + 1..];
        if value.chars().count() > MAX_ENV_VALUE_CHARS || has_control_chars(value) {
            out.bad_values.push(key.to_owned());
            continue;
        }
        out.vars.insert(key.to_owned(), value.to_owned());
    }
    out.too_many = out.vars.len() > MAX_ENV_VARS_PER_TABLE;
    out
}

pub fn env_text_error(parsed: &ParsedEnvText) -> Option<String> {
    if !parsed.invalid.is_empty() {
        return Some(tf(
            "变量名不合法：{names}（需以字母或下划线开头，只含字母、数字、下划线，最长 128）",
            "Invalid variable names: {names} (must start with a letter or underscore, contain only letters, digits and underscores, max 128)",
            &[("names", &parsed.invalid.join(", "))],
        ));
    }
    if !parsed.reserved.is_empty() {
        return Some(tf("这些变量名由宿主保留，不能配置：{names}", "These variable names are reserved by the host and cannot be configured: {names}", &[("names", &parsed.reserved.join(", "))]));
    }
    if !parsed.bad_values.is_empty() {
        return Some(tf("这些变量的值过长或含控制字符：{names}", "The values of these variables are too long or contain control characters: {names}", &[("names", &parsed.bad_values.join(", "))]));
    }
    if parsed.too_many {
        return Some(tf("一个工作区最多 {max} 条变量", "A workspace can hold at most {max} variables", &[("max", &MAX_ENV_VARS_PER_TABLE)]));
    }
    None
}

/// `validate_execution_environments` at the field: what the SSH dialog's Save checks, or the parsed port.
pub fn validate_ssh_machine(name: &str, host: &str, identity: &str, port: &str, creating: bool, machine_count: usize) -> Result<u16, String> {
    let (name, host, identity) = (name.trim(), host.trim(), identity.trim());
    if name.is_empty() || host.is_empty() {
        return Err(t("名称与主机不能为空", "Name and host must not be empty").to_owned());
    }
    if name.chars().count() > MAX_MACHINE_NAME_CHARS {
        return Err(tf("名称最长 {max} 个字符", "The name can be at most {max} characters", &[("max", &MAX_MACHINE_NAME_CHARS)]));
    }
    if host.chars().any(char::is_whitespace) || host.starts_with('-') {
        return Err(t("主机地址不能包含空白或以 - 开头", "The host must not contain whitespace or start with -").to_owned());
    }
    if host.chars().count() > MAX_HOST_CHARS || has_control_chars(host) {
        return Err(tf("主机地址过长或含控制字符（最长 {max}）", "The host is too long or contains control characters (max {max})", &[("max", &MAX_HOST_CHARS)]));
    }
    if identity.chars().count() > MAX_PATH_FIELD_CHARS || has_control_chars(identity) {
        return Err(tf("身份文件路径过长或含控制字符（最长 {max}）", "The identity file path is too long or contains control characters (max {max})", &[("max", &MAX_PATH_FIELD_CHARS)]));
    }
    if creating && machine_count >= MAX_SSH_MACHINES {
        return Err(tf("最多只能登记 {max} 台 SSH 机器", "At most {max} SSH machines can be registered", &[("max", &MAX_SSH_MACHINES)]));
    }
    let port = port.trim();
    if port.is_empty() {
        return Ok(0);
    }
    port.parse::<u16>().map_err(|_| t("端口必须是 0–65535 的整数", "The port must be an integer between 0 and 65535").to_owned())
}

/// `compactUrlLabel` of `DependencySettings`.
pub fn compact_url_label(value: &str) -> String {
    let without = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://")).unwrap_or(value);
    let host = without.split(['/', '?', '#']).next().unwrap_or(without).to_lowercase();
    if host == "github.com" {
        let path = without.split_once('/').map_or("", |(_, p)| p).split(['?', '#']).next().unwrap_or("").trim_matches('/');
        return if path.is_empty() { host } else { path.to_owned() };
    }
    host
}

/// `TOOL_NAME_PATTERN` of `DependencySettings`.
pub fn is_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_lines_keep_everything_after_the_first_equals() {
        let p = parse_env_text("A=1=2\r\n  \nB = spaced\nbad name=x\nNOVALUE\nBASH_ENV=x\nC=a\u{7}b");
        assert_eq!(p.vars.get("A").unwrap(), "1=2");
        assert_eq!(p.vars.get("B").unwrap(), " spaced");
        assert_eq!(p.invalid, ["bad name", "NOVALUE"]);
        assert_eq!(p.reserved, ["BASH_ENV"]);
        assert_eq!(p.bad_values, ["C"]);
        assert!(env_text_error(&p).is_some());
    }

    #[test]
    fn too_many_variables_is_a_problem_of_its_own() {
        let text: String = (0..=MAX_ENV_VARS_PER_TABLE).map(|i| format!("V{i}=1\n")).collect();
        assert!(parse_env_text(&text).too_many);
    }

    #[test]
    fn reserved_names_cover_the_private_prefixes() {
        assert!(reserved_env_var_name("ENV"));
        assert!(reserved_env_var_name("mewrk_e2e_x"));
        assert!(reserved_env_var_name("VITE_BROWSER_DEV"));
        assert!(!reserved_env_var_name("MEWRK_OTHER"));
    }

    #[test]
    fn the_ssh_form_says_what_the_host_would_refuse() {
        assert!(validate_ssh_machine("", "h", "", "", true, 0).is_err());
        assert!(validate_ssh_machine("n", "-h", "", "", true, 0).is_err());
        assert!(validate_ssh_machine("n", "a b", "", "", true, 0).is_err());
        assert!(validate_ssh_machine("n", "h", "", "70000", true, 0).is_err());
        assert!(validate_ssh_machine("n", "h", "", "", true, MAX_SSH_MACHINES).is_err());
        assert!(validate_ssh_machine("n", "h", "", "", false, MAX_SSH_MACHINES).is_ok());
        assert_eq!(validate_ssh_machine("n", "h", "", " 2222 ", true, 0), Ok(2222));
    }

    #[test]
    fn tool_links_are_compact() {
        assert_eq!(compact_url_label("https://github.com/nodejs/node"), "nodejs/node");
        assert_eq!(compact_url_label("https://nodejs.org/en"), "nodejs.org");
        assert_eq!(compact_url_label("https://github.com/"), "github.com");
    }

    #[test]
    fn tool_names_are_identifiers() {
        assert!(is_tool_name("node_18-lts"));
        assert!(!is_tool_name("bad name"));
        assert!(!is_tool_name("1bad"));
    }
}
