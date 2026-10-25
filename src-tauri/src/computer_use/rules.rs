//! 原生功能补丁规则：属性名与字符串字面量作骨架，minify 名正则捕获后回填。
//! 两端主进程 bundle 划分不同但结构一致，renderer 资源逐字节相同，故规则共用。

use std::sync::OnceLock;

use anyhow::{ensure, Context, Result};
use regex::{Captures, Regex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Windows,
    Macos,
}

pub const COMPUTER_USE: u8 = 1;
pub const SIMULATORS: u8 = 2;
pub const PREWARM: u8 = 4;
pub const FAST_STATUS: u8 = 8;

/// 配置函数里每个功能注入的字段。顺序决定输出字节，必须与已验证记录一致
type FieldSet = (u8, &'static [(&'static str, &'static str)]);

const MACOS_FIELDS: &[FieldSet] = &[
    (
        COMPUTER_USE,
        &[
            ("1291166712", "{enabled:!0,appScopedEnabled:!0}"),
            ("1346958739", "{major:27,minor:9999}"),
        ],
    ),
    (SIMULATORS, &[("3577536076", "!0"), ("1403324732", "!0")]),
    (PREWARM, &[("4237076858", "!0")]),
];

const WINDOWS_FIELDS: &[FieldSet] = &[
    (
        COMPUTER_USE,
        &[("1291166712", "{enabled:!0}"), ("40173473", "!0")],
    ),
    (PREWARM, &[("4237076858", "!0")]),
];

impl Platform {
    fn fields(self) -> &'static [FieldSet] {
        match self {
            Platform::Macos => MACOS_FIELDS,
            Platform::Windows => WINDOWS_FIELDS,
        }
    }

    /// 支持的功能位；主进程没有模拟器开关的平台不会假装能装
    pub fn supported(self) -> u8 {
        self.fields().iter().fold(FAST_STATUS, |mask, (bit, _)| mask | bit)
    }
}

fn regex(pattern: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("规则正则"))
}

macro_rules! rule {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static Regex {
            static CELL: OnceLock<Regex> = OnceLock::new();
            regex($pattern, &CELL)
        }
    };
}

/// 恰好命中一次才可信：零处说明结构已漂移，多处说明骨架太松会误伤。
/// regex crate 没有反向引用，同一变量的多处出现各自命名，由 aliases 要求同名
fn only<'t>(
    re: &Regex,
    text: &'t str,
    aliases: &[&[&str]],
    label: &str,
) -> Result<Captures<'t>> {
    let mut hits = re.captures_iter(text).filter(|caps| {
        aliases.iter().all(|group| {
            group
                .windows(2)
                .all(|pair| caps.name(pair[0]).map(|m| m.as_str()) == caps.name(pair[1]).map(|m| m.as_str()))
        })
    });
    let first = hits.next().with_context(|| format!("{label}未命中"))?;
    ensure!(hits.next().is_none(), "{label}命中不唯一");
    Ok(first)
}

fn splice(text: &str, edits: &mut Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = text.to_owned();
    for (start, end, new) in edits.iter() {
        out.replace_range(start..end, new);
    }
    out
}

rule!(
    config_pristine,
    concat!(
        r"function (?P<name>[\w$]+)\((?P<arg>[\w$]+)\)\{return (?P<expr>(?P<a2>[\w$]+)\.selfHostedSessions\?",
        r"(?P<a3>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+:(?P<a4>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+)\}",
    )
);

rule!(
    config_local,
    concat!(
        r"function (?P<name>[\w$]+)\((?P<arg>[\w$]+)\)\{let (?P<local>[\w$]+)=",
        r"(?P<expr>(?P<a2>[\w$]+)\.selfHostedSessions\?(?P<a3>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+:",
        r"(?P<a4>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+);return\{\.\.\.(?P<local2>[\w$]+),",
        r"(?P<fields>\d+:[\w$]+\([^()]*\)(?:,\d+:[\w$]+\([^()]*\))*)\}\}",
    )
);

rule!(
    config_spread,
    concat!(
        r"function (?P<name>[\w$]+)\((?P<arg>[\w$]+)\)\{return\{\.\.\.\((?P<expr>(?P<a2>[\w$]+)\.selfHostedSessions\?",
        r"(?P<a3>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+:(?P<a4>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+)\),",
        r"(?P<fields>\d+:[\w$]+\([^()]*\)(?:,\d+:[\w$]+\([^()]*\))*)\}\}",
    )
);

rule!(
    force_helper,
    concat!(
        r"function (?P<name>[\w$]+)\((?P<arg>[\w$]+)\)\{return\{value:(?P<value>[\w$]+),",
        r#"on:!!(?P<on>[\w$]+),off:!(?P<off>[\w$]+),source:"force",experiment:null,experimentResult:null\}\}"#,
    )
);

rule!(field_entry, r"(?P<id>\d+):(?P<helper>[\w$]+)\((?P<value>[^()]*)\)");

const CONFIG_ALIASES: &[&[&str]] = &[&["arg", "a2", "a3", "a4"]];

struct Config<'t> {
    name: String,
    arg: String,
    expr: String,
    fields: Option<(usize, usize, &'t str)>,
    span: (usize, usize),
    local: bool,
}

fn helper_name(text: &str) -> Result<String> {
    let caps = only(
        force_helper(),
        text,
        &[&["arg", "value", "on", "off"]],
        "功能开关辅助函数",
    )?;
    Ok(caps["name"].to_string())
}

fn config<'t>(text: &'t str) -> Result<Config<'t>> {
    let mut found: Vec<Config> = Vec::new();
    for (re, local) in [
        (config_pristine(), false),
        (config_local(), true),
        (config_spread(), false),
    ] {
        let aliases: Vec<&[&str]> = if local {
            CONFIG_ALIASES.iter().copied().chain([&["local", "local2"][..]]).collect()
        } else {
            CONFIG_ALIASES.to_vec()
        };
        for caps in re.captures_iter(text).filter(|caps| {
            aliases.iter().all(|group| {
                group.windows(2).all(|pair| {
                    caps.name(pair[0]).map(|m| m.as_str()) == caps.name(pair[1]).map(|m| m.as_str())
                })
            })
        }) {
            let whole = caps.get(0).context("配置函数范围缺失")?;
            found.push(Config {
                name: caps["name"].to_string(),
                arg: caps["arg"].to_string(),
                expr: caps["expr"].to_string(),
                fields: caps.name("fields").map(|m| (m.start(), m.end(), m.as_str())),
                span: (whole.start(), whole.end()),
                local,
            });
        }
    }
    ensure!(found.len() == 1, "原生配置函数命中 {} 处", found.len());
    Ok(found.remove(0))
}

/// 已注入的字段必须整组出现，且只能来自本平台的已知组合
fn config_mask(text: &str, platform: Platform) -> Result<u8> {
    let helper = helper_name(text)?;
    let config = config(text)?;
    let Some((_, _, raw)) = config.fields else {
        return Ok(0);
    };
    let mut present: Vec<(String, String)> = Vec::new();
    for caps in field_entry().captures_iter(raw) {
        ensure!(caps["helper"] == *helper, "配置字段引用了其他辅助函数");
        present.push((caps["id"].to_string(), caps["value"].to_string()));
    }
    let mut mask = 0;
    for (bit, fields) in platform.fields() {
        let hits = fields
            .iter()
            .filter(|(id, value)| present.iter().any(|(i, v)| i == id && v == value))
            .count();
        if hits == 0 {
            continue;
        }
        ensure!(hits == fields.len(), "原生功能字段只注入了一半");
        mask |= bit;
        present.retain(|(i, v)| !fields.iter().any(|(id, value)| i == id && v == value));
    }
    ensure!(present.is_empty(), "配置函数含有未知的原生字段");
    Ok(mask)
}

/// 局部变量名避开表达式里出现过的符号，输出才与已验证记录逐字节一致
fn free_name(expr: &str, arg: &str) -> Result<char> {
    "tnriosacludbfg"
        .chars()
        .find(|c| {
            let name = c.to_string();
            arg != name && !regex_word(expr).contains(&name)
        })
        .context("找不到可用的局部变量名")
}

fn regex_word(text: &str) -> Vec<String> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(r"[\w$]+", &CELL)
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn patch_config(text: &str, platform: Platform, mask: u8) -> Result<String> {
    let helper = helper_name(text)?;
    let config = config(text)?;
    ensure!(config.fields.is_none(), "必须从官方配置函数生成补丁");
    let mut fields = Vec::new();
    for (bit, entries) in platform.fields() {
        if mask & bit == 0 {
            continue;
        }
        fields.extend(entries.iter().map(|(id, value)| format!("{id}:{helper}({value})")));
    }
    ensure!(
        mask & !platform.supported() == 0,
        "当前平台的主进程没有所选原生功能的开关"
    );
    if fields.is_empty() {
        return Ok(text.to_owned());
    }
    let joined = fields.join(",");
    let replacement = if platform == Platform::Macos {
        let local = free_name(&config.expr, &config.arg)?;
        format!(
            "function {}({}){{let {local}={};return{{...{local},{joined}}}}}",
            config.name, config.arg, config.expr
        )
    } else {
        format!(
            "function {}({}){{return{{...({}),{joined}}}}}",
            config.name, config.arg, config.expr
        )
    };
    let _ = config.local;
    let mut edits = vec![(config.span.0, config.span.1, replacement)];
    Ok(splice(text, &mut edits))
}

rule!(
    prewarm_entry,
    concat!(
        r#"async sparePrewarmSupport\(\)\{if\(!(?P<flags>[\w$]+(?:\.[\w$]+)?)\("4237076858"\)\)"#,
        r#"return\{unsupported:"gate_off"\};"#,
        r#"(?P<gate>if\((?P<deploy>[\w$]+(?:\.[\w$]+)?)\(\)\.type!=="1p"\)return\{unsupported:"third_party"\};)?"#,
    )
);

pub fn prewarm_installed(text: &str) -> Result<bool> {
    let caps = only(prewarm_entry(), text, &[], "原生预热入口")?;
    Ok(caps.name("gate").is_none())
}

pub fn patch_prewarm(text: &str) -> Result<String> {
    let caps = only(prewarm_entry(), text, &[], "原生预热入口")?;
    let gate = caps.name("gate").context("原生预热门槛已移除")?;
    let mut edits = vec![(gate.start(), gate.end(), String::new())];
    Ok(splice(text, &mut edits))
}

rule!(
    fast_parse,
    r#"for\(let (?P<v>[\w$]+) of\["badge","tooltip","notice","thinking"(?P<fast>,"fast_mode")?(?P<close>\]\))"#
);

rule!(
    fast_emit,
    concat!(
        r"(?P<g>[\w$]+)=\{id:(?P<model>[\w$]+)\.id,name:[\w$]+,description:[\w$]+,",
        r"thinking:[\w$]+\((?P<surface>[\w$]+),(?P<model2>[\w$]+)\.id,[\w$]+\),",
        r#"(?P<added>\.\.\.(?P<surface2>[\w$]+)==="code"&&(?P<srv1>[\w$]+)\?\.fast_mode!==void 0&&"#,
        r"\{fast_mode:(?P<srv2>[\w$]+)\.fast_mode\},)?",
        r#"(?P<tail>\.\.\.[\w$]+!==void 0&&\{section:"overflow")"#,
    )
);

rule!(
    fast_server,
    concat!(
        r"let\{model:(?P<model>[\w$]+),catalog:[\w$]+,hybrid:[\w$]+,",
        r"server:(?P<server>[\w$]+),primaryId:[\w$]+,label:[\w$]+\}=",
    )
);

rule!(
    fast_capability,
    concat!(
        r#"function [\w$]+\((?P<arg>[\w$]+)\)\{let (?P<wrap>[\w$]+)=(?P<wa>[\w$]+)=>(?P<wa2>[\w$]+)\?"available":"blocked_by_org_admin","#,
        r"(?P<code>[\w$]+)=(?P<arg2>[\w$]+)\.codeSurface\?\.enabled!==!1;return\{(?P<body>[^{}]{0,4000}?)",
        r#"claude_code_fast_mode:(?P<value>"blocked_by_platform"|(?P<wrap2>[\w$]+)\((?P<code2>[\w$]+)&&(?P<acc>[\w$]+)\(\)\.allowsLocalSpawn\))"#,
    )
);

rule!(
    fast_project,
    concat!(
        r"function [\w$]+\((?P<arg>[\w$]+)\)\{let (?P<t>[\w$]+)=(?P<guard>[\w$]+)\((?P<arg2>[\w$]+)\);",
        r"if\(!(?P<t2>[\w$]+)\)return;let (?P<n>[\w$]+)=\{\};[^{}]{0,4000}?",
        r"let (?P<o>[\w$]+)=[\w$]+\((?P<t3>[\w$]+)\.thinking\);return (?P<o2>[\w$]+)&&\((?P<n2>[\w$]+)\.thinking=(?P<o3>[\w$]+)\),",
        r"(?P<added>(?P<guard2>[\w$]+)\((?P<t4>[\w$]+)\.fast_mode\)&&\((?P<n3>[\w$]+)\.fast_mode=(?P<t5>[\w$]+)\.fast_mode\),)?",
        r"Object\.keys\((?P<n4>[\w$]+)\)\.length>0\?(?P<n5>[\w$]+):void 0",
    )
);

rule!(local_spawn, r"(?P<acc>[\w$]+)\(\)\.allowsLocalSpawn");

/// 能力访问器在全文多处出现，名字必须一致才敢回填
fn accessor(text: &str) -> Result<String> {
    let mut names: Vec<_> = local_spawn()
        .captures_iter(text)
        .map(|caps| caps["acc"].to_string())
        .collect();
    names.sort();
    names.dedup();
    ensure!(names.len() == 1, "本地会话能力访问器不唯一");
    Ok(names.remove(0))
}

const EMIT_ALIASES: &[&[&str]] = &[&["model", "model2"]];
const CAP_ALIASES: &[&[&str]] = &[&["arg", "arg2"], &["wa", "wa2"]];
const PROJECT_ALIASES: &[&[&str]] = &[
    &["arg", "arg2"],
    &["t", "t2", "t3"],
    &["o", "o2", "o3"],
    &["n", "n2", "n4", "n5"],
];

/// emit 与 server 解构必须属于同一个函数，否则 server 变量名回填到别处
const EMIT_CONTEXT: usize = 4096;

struct FastParts<'t> {
    parse: Captures<'t>,
    emit: Captures<'t>,
    server: String,
    capability: Captures<'t>,
    project: Captures<'t>,
}

fn fast_parts<'t>(text: &'t str) -> Result<FastParts<'t>> {
    let parse = only(fast_parse(), text, &[], "Fast 能力解析入口")?;
    let emit = only(fast_emit(), text, EMIT_ALIASES, "Fast 模型条目入口")?;
    let server = only(fast_server(), text, &[], "Fast 模型服务端字段")?;
    let emit_start = emit.get(0).context("Fast 条目范围缺失")?.start();
    let server_start = server.get(0).context("Fast 字段范围缺失")?.start();
    ensure!(
        server["model"] == emit["model"]
            && emit_start > server_start
            && emit_start - server_start < EMIT_CONTEXT,
        "Fast 模型条目与服务端字段不在同一处"
    );
    Ok(FastParts {
        parse,
        emit,
        server: server["server"].to_string(),
        capability: only(fast_capability(), text, CAP_ALIASES, "Fast 能力映射")?,
        project: only(fast_project(), text, PROJECT_ALIASES, "Fast 配置投影")?,
    })
}

/// None 表示这份主进程根本不含 Fast 能力映射，与"映射存在但未安装"要分开
pub fn fast_main_installed(text: &str) -> Result<Option<bool>> {
    if !text.contains("claude_code_fast_mode:") {
        return Ok(None);
    }
    let parts = fast_parts(text)?;
    let flags = [
        parts.parse.name("fast").is_some(),
        parts.emit.name("added").is_some(),
        &parts.capability["value"] != "\"blocked_by_platform\"",
        parts.project.name("added").is_some(),
    ];
    ensure!(
        flags.iter().all(|f| *f) || flags.iter().all(|f| !f),
        "Fast 原生映射只改了一部分"
    );
    if !flags[0] {
        return Ok(Some(false));
    }
    ensure!(
        parts.emit["surface2"] == parts.emit["surface"]
            && parts.emit["srv1"] == *parts.server
            && parts.emit["srv2"] == *parts.server,
        "Fast 模型条目引用了其他变量"
    );
    ensure!(
        parts.capability["wrap2"] == parts.capability["wrap"]
            && parts.capability["code2"] == parts.capability["code"]
            && parts.capability["acc"] == *accessor(text)?,
        "Fast 能力映射引用了其他变量"
    );
    ensure!(
        parts.project["guard2"] == parts.project["guard"]
            && parts.project["t4"] == parts.project["t"]
            && parts.project["t5"] == parts.project["t"]
            && parts.project["n3"] == parts.project["n"],
        "Fast 配置投影引用了其他变量"
    );
    Ok(Some(true))
}

pub fn patch_fast_main(text: &str) -> Result<String> {
    ensure!(
        fast_main_installed(text)?.context("当前主进程没有 Fast 能力映射")? == false,
        "Fast 原生映射已安装"
    );
    let accessor = accessor(text)?;
    let parts = fast_parts(text)?;
    let close = parts.parse.name("close").context("解析入口范围缺失")?;
    let emit = parts.emit.name("tail").context("条目范围缺失")?;
    let value = parts.capability.name("value").context("能力范围缺失")?;
    let keys = parts.project.name("n4").context("投影范围缺失")?;
    let keys_start = keys.start() - "Object.keys(".len();
    let mut edits = vec![
        (close.start(), close.start(), ",\"fast_mode\"".to_string()),
        (
            emit.start(),
            emit.start(),
            format!(
                "...{}===\"code\"&&{}?.fast_mode!==void 0&&{{fast_mode:{}.fast_mode}},",
                &parts.emit["surface"], parts.server, parts.server
            ),
        ),
        (
            value.start(),
            value.end(),
            format!(
                "{}({}&&{accessor}().allowsLocalSpawn)",
                &parts.capability["wrap"], &parts.capability["code"]
            ),
        ),
        (
            keys_start,
            keys_start,
            format!(
                "{}({}.fast_mode)&&({}.fast_mode={}.fast_mode),",
                &parts.project["guard"], &parts.project["t"], &parts.project["n"], &parts.project["t"]
            ),
        ),
    ];
    Ok(splice(text, &mut edits))
}

rule!(
    fast_state,
    concat!(
        r#"(?P<res>[\w$]+)=(?P<pick>[\w$]+)\?\?\((?P<remote>[\w$]+)\?(?P<st>[\w$]+)==="on"\|\|(?P<st2>[\w$]+)==="cooldown":"#,
        r#"(?P<cred>[\w$]+)\?!1:(?P<added>(?P<local>[\w$]+)&&\["on","off","cooldown"\]\.includes\((?P<st3>[\w$]+)\)\?"#,
        r#"(?P<st4>[\w$]+)!=="off":)?(?P<agent>[\w$]+)\?\?\("#,
    )
);

rule!(
    fast_credits,
    r"(?P<cred>[\w$]+)=(?P<local>[\w$]+)&&[\w$]+\?\.fastModeOffByCredits===!0"
);

rule!(
    fast_agent,
    r"(?P<agent>[\w$]+)=(?P<local>[\w$]+)\?[\w$]+\?\.fastModeByAgent:void 0"
);

/// 本地会话标志取自同一函数里的 credits / agent 声明，避免把远端分支也改掉
fn fast_state_parts<'t>(text: &'t str) -> Result<(Captures<'t>, String)> {
    let state = only(fast_state(), text, &[&["st", "st2"]], "Fast 状态表达式")?;
    let credits = only(fast_credits(), text, &[], "Fast 额度判定")?;
    let agent = only(fast_agent(), text, &[], "Fast 代理判定")?;
    ensure!(
        credits["cred"] == state["cred"]
            && agent["agent"] == state["agent"]
            && credits["local"] == agent["local"],
        "Fast 状态表达式上下文不一致"
    );
    Ok((state, credits["local"].to_string()))
}

pub fn fast_state_installed(text: &str) -> Result<bool> {
    let (state, local) = fast_state_parts(text)?;
    if state.name("added").is_none() {
        return Ok(false);
    }
    ensure!(
        state["local"] == *local && state["st3"] == state["st"] && state["st4"] == state["st"],
        "Fast 状态补丁引用了其他变量"
    );
    Ok(true)
}

pub fn patch_fast_state(text: &str) -> Result<String> {
    let (state, local) = fast_state_parts(text)?;
    ensure!(state.name("added").is_none(), "Fast 状态补丁已安装");
    let agent = state.name("agent").context("Fast 代理范围缺失")?;
    let st = &state["st"];
    let mut edits = vec![(
        agent.start(),
        agent.start(),
        format!("{local}&&[\"on\",\"off\",\"cooldown\"].includes({st})?{st}!==\"off\":"),
    )];
    Ok(splice(text, &mut edits))
}

const CHINESE: &str = ",\"zh-Hans\",\"zh-Hant\"";

rule!(
    locale_list,
    r#"(?P<var>[\w$]+)=\[(?P<items>"en-US"(?:,"[A-Za-z0-9-]+")*)\]"#
);

rule!(
    locale_resolver,
    r"function (?P<fn>[\w$]+)\((?P<a>[\w$]+),(?P<b>[\w$]+)=(?P<list>[\w$]+)\)\{"
);

/// JS 里带字符串的函数体不能靠括号配对硬数，逐字符跳过引号与转义
fn body_end(text: &str, open: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' | b'`' => quote = Some(c),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i + 1);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    anyhow::bail!("语言函数体未闭合")
}

struct Locale {
    resolver: String,
    fallback_args: (String, String),
    fallback_body: (usize, usize),
    lists: Vec<(usize, usize)>,
    patched: bool,
}

/// 官方有两个解析器：认中文的那个按 zh- 前缀归并，白名单那个只做精确匹配
fn locale(text: &str) -> Result<Locale> {
    let lists: Vec<_> = locale_list()
        .captures_iter(text)
        .filter_map(|caps| {
            let items = caps.name("items")?;
            Some((caps["var"].to_string(), items.start(), items.end()))
        })
        .collect();
    let mut chinese_aware = Vec::new();
    let mut plain = Vec::new();
    for caps in locale_resolver().captures_iter(text) {
        let Some(list) = lists.iter().find(|(var, _, _)| *var == caps["list"]) else {
            continue;
        };
        let open = caps.get(0).context("语言函数范围缺失")?.end() - 1;
        let end = body_end(text, open)?;
        let body = &text[open + 1..end - 1];
        let entry = (
            caps["fn"].to_string(),
            caps["a"].to_string(),
            caps["b"].to_string(),
            (list.1, list.2),
            open + 1,
            end - 1,
        );
        if body.contains("startsWith(\"zh-\")") {
            chinese_aware.push(entry);
        } else if body.contains("new Map") || body.starts_with("return ") {
            plain.push(entry);
        }
    }
    ensure!(
        chinese_aware.len() == 1 && plain.len() == 1,
        "语言解析入口不唯一"
    );
    let (resolver, _, _, resolver_list, _, _) = chinese_aware.remove(0);
    let (_, arg_a, arg_b, fallback_list, open, end) = plain.remove(0);
    let mut lists = vec![resolver_list];
    if fallback_list != resolver_list {
        lists.push(fallback_list);
    }
    let delegated = format!("return {resolver}({arg_a},{arg_b})");
    let patched_lists = lists
        .iter()
        .filter(|(start, end)| text[*start..*end].ends_with(&CHINESE[1..]))
        .count();
    let patched_body = text[open..end] == delegated;
    ensure!(
        patched_lists == lists.len() && patched_body || patched_lists == 0 && !patched_body,
        "语言补丁只应用了一部分"
    );
    ensure!(
        patched_lists > 0 || !lists.iter().any(|(s, e)| text[*s..*e].contains("\"zh-Han")),
        "语言白名单已包含中文但缺少解析改动"
    );
    Ok(Locale {
        resolver,
        fallback_args: (arg_a, arg_b),
        fallback_body: (open, end),
        lists,
        patched: patched_body,
    })
}

pub fn language_installed(text: &str) -> Result<bool> {
    Ok(locale(text)?.patched)
}

pub fn patch_language(text: &str) -> Result<String> {
    let locale = locale(text)?;
    ensure!(!locale.patched, "语言补丁已安装");
    let mut edits = vec![(
        locale.fallback_body.0,
        locale.fallback_body.1,
        format!(
            "return {}({},{})",
            locale.resolver, locale.fallback_args.0, locale.fallback_args.1
        ),
    )];
    edits.extend(locale.lists.iter().map(|(_, end)| (*end, *end, CHINESE.to_string())));
    Ok(splice(text, &mut edits))
}

const MAX_ASSET: u64 = 64 * 1024 * 1024;

type Asset = (std::path::PathBuf, String);

/// renderer 的 chunk 名带内容哈希，只能按内容定位；命中必须唯一。
/// 先按字面标记筛字节，几千个 chunk 里只有一两个需要跑正则
fn locate_asset(
    dir: &std::path::Path,
    marker: &str,
    accept: impl Fn(&str) -> bool,
    label: &str,
) -> Result<Option<Asset>> {
    let mut found = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("读取{label}目录"))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file() && !meta.is_symlink() && meta.len() <= MAX_ASSET,
            "{label}不是安全的普通文件"
        );
        let bytes = std::fs::read(&path)?;
        if memchr::memmem::find(&bytes, marker.as_bytes()).is_none() {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if !accept(&text) {
            continue;
        }
        ensure!(found.is_none(), "{label}入口不唯一，拒绝修改");
        found = Some((path, text));
    }
    Ok(found)
}

pub fn fast_state_asset(dir: &std::path::Path) -> Result<Option<Asset>> {
    locate_asset(dir, "fastModeOffByCredits", |text| fast_state_parts(text).is_ok(), "Fast 状态")
}

pub fn language_asset(dir: &std::path::Path) -> Result<Option<Asset>> {
    locate_asset(dir, "startsWith(\"zh-\")", |text| locale(text).is_ok(), "语言")
}

/// 已装原生 Fast 状态补丁的 renderer 文件；读不出或没装都是 None
#[cfg(any(test, windows))]
pub fn installed_fast_state(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let (path, text) = fast_state_asset(dir).ok()??;
    fast_state_installed(&text).ok()?.then_some(path)
}

pub fn main_mask(text: &str, platform: Platform) -> Result<u8> {
    let mut mask = config_mask(text, platform)?;
    if fast_main_installed(text)? == Some(true) {
        mask |= FAST_STATUS;
    }
    Ok(mask)
}

pub fn patch_main(text: &str, platform: Platform, mask: u8) -> Result<String> {
    ensure!(main_mask(text, platform)? == 0, "必须从官方主进程生成补丁");
    let mut out = patch_config(text, platform, mask)?;
    if mask & FAST_STATUS != 0 {
        out = patch_fast_main(&out)?;
    }
    ensure!(main_mask(&out, platform)? == mask, "主进程补丁回读不一致");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELPER: &str = concat!(
        r#"function fv(e){return{value:e,on:!!e,off:!e,source:"force","#,
        r#"experiment:null,experimentResult:null}}"#,
    );
    const CONFIG: &str = concat!(
        "function pick(e){return e.selfHostedSessions?e.builtinBrowserEnabled?A:B:",
        "e.builtinBrowserEnabled?C:D}",
    );

    fn source() -> String {
        format!("var A=1,B=2,C=3,D=4;{HELPER}{CONFIG}")
    }

    #[test]
    fn config_fields_follow_the_platform_and_round_trip() {
        for platform in [Platform::Windows, Platform::Macos] {
            for mask in 0..8u8 {
                if mask & !platform.supported() != 0 {
                    assert!(patch_config(&source(), platform, mask).is_err(), "{mask}");
                    continue;
                }
                let patched = patch_config(&source(), platform, mask).unwrap();
                assert_eq!(config_mask(&patched, platform).unwrap(), mask, "{mask:?}");
                // 空组合不改动原文，只有写入过字段的输出才该拒绝二次注入
                assert_eq!(patch_config(&patched, platform, mask).is_err(), mask != 0);
            }
        }
        assert_eq!(Platform::Windows.supported() & SIMULATORS, 0);
        assert_ne!(Platform::Macos.supported() & SIMULATORS, 0);
    }

    #[test]
    fn renamed_identifiers_still_match_but_changed_semantics_do_not() {
        let renamed = source()
            .replace("fv(", "$helper$(")
            .replace("(e)", "($a$)")
            .replace("e.", "$a$.")
            .replace("value:e", "value:$a$")
            .replace("!!e", "!!$a$")
            .replace("!e,", "!$a$,");
        assert_eq!(config_mask(&renamed, Platform::Macos).unwrap(), 0);
        assert!(patch_config(&renamed, Platform::Macos, COMPUTER_USE).is_ok());
        for (from, to) in [
            ("on:!!e", "on:!e"),
            ("source:\"force\"", "source:\"remote\""),
            ("e.selfHostedSessions", "other.selfHostedSessions"),
        ] {
            assert!(config_mask(&source().replace(from, to), Platform::Macos).is_err(), "{from}");
        }
        assert!(config_mask(&format!("{}{}", source(), source()), Platform::Macos).is_err());
    }

    #[test]
    fn foreign_fields_are_never_reported_as_a_known_combination() {
        let patched = patch_config(&source(), Platform::Macos, COMPUTER_USE).unwrap();
        let extra = patched.replace("1346958739:fv({major:27,minor:9999})", "1346958739:fv({major:27,minor:9999}),9:fv(!0)");
        assert!(config_mask(&extra, Platform::Macos).is_err());
        let half = patched.replace(",1346958739:fv({major:27,minor:9999})", "");
        assert!(config_mask(&half, Platform::Macos).is_err());
    }

    const PREWARM_SOURCE: &str = concat!(
        r#"async sparePrewarmSupport(){if(!t.x0("4237076858"))return{unsupported:"gate_off"};"#,
        r#"if(t.Ret().type!=="1p")return{unsupported:"third_party"};let e;}"#,
    );

    #[test]
    fn prewarm_removes_only_the_deployment_gate() {
        assert!(!prewarm_installed(PREWARM_SOURCE).unwrap());
        let patched = patch_prewarm(PREWARM_SOURCE).unwrap();
        assert!(prewarm_installed(&patched).unwrap());
        assert!(patched.contains("gate_off"));
        assert!(!patched.contains("third_party"));
        assert!(patch_prewarm(&patched).is_err());
    }

    fn fast_source() -> String {
        concat!(
            r#"let{model:i,catalog:a,hybrid:o,server:s,primaryId:l,label:u}=n;"#,
            r#"g={id:i.id,name:f,description:h,thinking:aE(t,i.id,e),...l!==void 0&&{section:"overflow",...T(i.id)};"#,
            r#"for(let t of["badge","tooltip","notice","thinking"])V(e[t])&&(n[t]=e[t]);"#,
            r#"function cap(e){let t=e=>e?"available":"blocked_by_org_admin",n=e.codeSurface?.enabled!==!1;"#,
            r#"return{chat:t(x(e)),claude_code_fast_mode:"blocked_by_platform",cowork:t(y(e))};}"#,
            r#"function proj(e){let t=DT(e);if(!t)return;let n={};typeof t.name=="string"&&(n.name=t.name);"#,
            r#"let o=Son(t.thinking);return o&&(n.thinking=o),Object.keys(n).length>0?n:void 0}"#,
            r#"var X={get allowsLocalSpawn(){return Xw().allowsLocalSpawn}};"#,
        )
        .to_string()
    }

    #[test]
    fn fast_mapping_is_all_or_nothing_and_keeps_native_gates() {
        let source = fast_source();
        assert_eq!(fast_main_installed(&source).unwrap(), Some(false));
        let patched = patch_fast_main(&source).unwrap();
        assert_eq!(fast_main_installed(&patched).unwrap(), Some(true));
        assert!(patched.contains(r#""thinking","fast_mode"]"#));
        assert!(patched.contains(r#"...t==="code"&&s?.fast_mode!==void 0&&{fast_mode:s.fast_mode},"#));
        assert!(patched.contains("claude_code_fast_mode:t(n&&Xw().allowsLocalSpawn)"));
        assert!(patched.contains("DT(t.fast_mode)&&(n.fast_mode=t.fast_mode),Object.keys(n)"));
        assert!(patch_fast_main(&patched).is_err());
        let half = patched.replace(r#""thinking","fast_mode"]"#, r#""thinking"]"#);
        assert!(fast_main_installed(&half).is_err());
        let foreign = patched.replace("{fast_mode:s.fast_mode}", "{fast_mode:other.fast_mode}");
        assert!(fast_main_installed(&foreign).is_err());
    }

    const STATE_SOURCE: &str = concat!(
        r#"let xe=w&&n?.fastModeOffByCredits===!0,Ze=w?n?.fastModeByAgent:void 0,"#,
        r#"nt=ge??(E?D==="on"||D==="cooldown":xe?!1:Ze??(u===null||!t&&K?!1:re??G)),rt=!W&&nt;"#,
    );

    #[test]
    fn fast_state_only_overrides_local_sessions() {
        assert!(!fast_state_installed(STATE_SOURCE).unwrap());
        let patched = patch_fast_state(STATE_SOURCE).unwrap();
        assert!(fast_state_installed(&patched).unwrap());
        assert!(patched.contains(r#"xe?!1:w&&["on","off","cooldown"].includes(D)?D!=="off":Ze??"#));
        assert!(patch_fast_state(&patched).is_err());
        let foreign = patched.replace(r#"w&&["on""#, r#"other&&["on""#);
        assert!(fast_state_installed(&foreign).is_err());
    }

    const LOCALE_SOURCE: &str = concat!(
        r#"var ay=["en-US","de-DE","fr-FR"];function vy(e,t=ay){let n=new Map;"#,
        r#"for(let t of e)if(t){let e=t.toLowerCase();if(e==="zh"||e.startsWith("zh-")){}}return ry}"#,
        r#"var by=["en-US","de-DE","fr-FR"];function xy(e,t=by){let n=new Map;"#,
        r#"for(let t of e)if(t){let e=t.toLowerCase(),r=n.get(e);if(r!==void 0)return r}return yy}"#,
    );

    #[test]
    fn chinese_delegates_to_the_official_resolver() {
        assert!(!language_installed(LOCALE_SOURCE).unwrap());
        let patched = patch_language(LOCALE_SOURCE).unwrap();
        assert!(language_installed(&patched).unwrap());
        assert_eq!(patched.matches("\"zh-Hans\",\"zh-Hant\"").count(), 2);
        assert!(patched.contains("function xy(e,t=by){return vy(e,t)}"));
        assert!(patch_language(&patched).is_err());
        let half = patched.replace("function xy(e,t=by){return vy(e,t)}", LOCALE_SOURCE.split("function xy").nth(1).map(|s| format!("function xy{s}")).unwrap().trim_end());
        assert!(language_installed(&half).is_err());
    }

    #[test]
    fn renderer_assets_are_found_by_content_and_must_be_unique() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fast_state_asset(dir.path()).unwrap().is_none());
        std::fs::write(dir.path().join("unrelated.js"), "fastModeOffByCredits only").unwrap();
        std::fs::write(dir.path().join("renamed-chunk.js"), STATE_SOURCE).unwrap();
        // Fast Mode 页留下的备份不是 .js，不能被当成第二份入口
        std::fs::write(dir.path().join("renamed-chunk.js.orig"), STATE_SOURCE).unwrap();
        let (path, text) = fast_state_asset(dir.path()).unwrap().unwrap();
        assert_eq!(path.file_name().unwrap(), "renamed-chunk.js");
        assert_eq!(text, STATE_SOURCE);
        assert!(language_asset(dir.path()).unwrap().is_none());
        std::fs::write(dir.path().join("duplicate.js"), STATE_SOURCE).unwrap();
        assert!(fast_state_asset(dir.path()).is_err());
    }

    #[test]
    fn installed_fast_state_reports_only_patched_renderers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(installed_fast_state(&dir.path().join("missing")), None);
        let path = dir.path().join("state.js");
        std::fs::write(&path, STATE_SOURCE).unwrap();
        assert_eq!(installed_fast_state(dir.path()), None);
        std::fs::write(&path, patch_fast_state(STATE_SOURCE).unwrap()).unwrap();
        assert_eq!(installed_fast_state(dir.path()), Some(path));
    }

    #[test]
    fn language_bodies_with_braces_in_strings_are_measured_correctly() {
        let source = LOCALE_SOURCE.replace(r#"return yy}"#, r#"return "}"+yy}"#);
        let patched = patch_language(&source).unwrap();
        assert!(patched.contains("function xy(e,t=by){return vy(e,t)}"));
        assert!(!patched.contains("\"}\"+yy"));
    }

    /// 用两端真实 chunk 验证同一套规则：CLAUDEPP_CHUNKS 指向导出的 <平台>-<入口>.js
    #[test]
    #[ignore = "需要官方主进程副本"]
    fn verified_builds_round_trip_on_both_platforms() {
        let dir = std::path::PathBuf::from(std::env::var_os("CLAUDEPP_CHUNKS").expect("缺少 chunk 目录"));
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).expect(name);
        for (platform, prefix) in [(Platform::Macos, "mac"), (Platform::Windows, "win")] {
            let main = read(&format!("{prefix}-main.js"));
            let supported = platform.supported();
            for mask in 0..=supported {
                if mask & !supported != 0 {
                    continue;
                }
                let patched = patch_main(&main, platform, mask).unwrap();
                assert_eq!(main_mask(&patched, platform).unwrap(), mask, "{prefix} {mask}");
                assert_eq!(patched == main, mask == 0, "{prefix} {mask}");
            }
            let prewarm = read(&format!("{prefix}-prewarm.js"));
            assert!(!prewarm_installed(&prewarm).unwrap());
            assert!(prewarm_installed(&patch_prewarm(&prewarm).unwrap()).unwrap());
        }
        // 结构化规则必须复现逐版本手写记录的每一种组合，否则 mac 既有安装会被判成外部修改
        let recipe = super::super::macos_recipe::get("3.6556.2").expect("内置 macOS 适配记录");
        let main = read("mac-main.js");
        for mask in 0..16u8 {
            let patched = patch_main(&main, Platform::Macos, mask).unwrap();
            assert_eq!(
                super::super::asar::hash(patched.as_bytes()),
                recipe.main.profile(mask).unwrap(),
                "mac profile {mask}"
            );
        }
    }

    /// renderer 两端逐字节相同，一份资源即可验证中文与 Fast 状态
    #[test]
    #[ignore = "需要官方 renderer 副本"]
    fn verified_renderer_assets_round_trip() {
        let dir = std::path::PathBuf::from(std::env::var_os("CLAUDEPP_RENDERER").expect("缺少 renderer 目录"));
        let mut language = 0;
        let mut state = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if locale(&text).is_ok() {
                language += 1;
                assert!(language_installed(&patch_language(&text).unwrap()).unwrap());
            }
            if fast_state_parts(&text).is_ok() {
                state += 1;
                assert!(fast_state_installed(&patch_fast_state(&text).unwrap()).unwrap());
            }
        }
        assert_eq!((language, state), (1, 1), "语言与 Fast 状态入口各应唯一");
    }

    #[test]
    fn main_patch_combines_config_and_fast_mapping() {
        let source = format!("{}{}", source(), fast_source());
        for mask in [0, COMPUTER_USE, FAST_STATUS, COMPUTER_USE | PREWARM | FAST_STATUS] {
            let patched = patch_main(&source, Platform::Windows, mask).unwrap();
            assert_eq!(main_mask(&patched, Platform::Windows).unwrap(), mask);
            if mask != 0 {
                assert!(patch_main(&patched, Platform::Windows, mask).is_err());
            }
        }
        assert!(patch_main(&source, Platform::Windows, SIMULATORS).is_err());
    }
}
