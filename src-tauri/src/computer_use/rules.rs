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
    // Windows 版不带 iOS 模拟器；Android 的 control 与 logs 工具各有一个开关
    (SIMULATORS, &[("4272007212", "!0"), ("2784298314", "!0")]),
    (PREWARM, &[("4237076858", "!0")]),
];

impl Platform {
    fn fields(self) -> &'static [FieldSet] {
        match self {
            Platform::Macos => MACOS_FIELDS,
            Platform::Windows => WINDOWS_FIELDS,
        }
    }

    /// Fast 状态不走配置函数字段，单独计入
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
    let mut matched = Vec::new();
    for caps in field_entry().captures_iter(raw) {
        matched.push(caps.get(0).context("配置字段范围缺失")?.as_str());
        ensure!(caps["helper"] == *helper, "配置字段引用了其他辅助函数");
        ensure!(!present.iter().any(|(id, _)| id == &caps["id"]), "配置字段重复");
        present.push((caps["id"].to_string(), caps["value"].to_string()));
    }
    ensure!(matched.join(",") == raw, "配置函数包含无法识别的字段");
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
    ensure!(
        mask & !platform.supported() == 0,
        "当前平台的主进程没有所选原生功能的开关"
    );
    let mut fields = Vec::new();
    for (bit, entries) in platform.fields() {
        if mask & bit == 0 {
            continue;
        }
        // 预热开关在独立模块中校验。
        if *bit != PREWARM {
            for (id, _) in entries.iter() {
                ensure!(
                    text.contains(&format!("\"{id}\"")),
                    "主进程里没有读取开关 {id} 的代码，结构可能已变化"
                );
            }
        }
        fields.extend(entries.iter().map(|(id, value)| format!("{id}:{helper}({value})")));
    }
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
    let mut edits = vec![(config.span.0, config.span.1, replacement)];
    Ok(splice(text, &mut edits))
}

rule!(
    prewarm_entry,
    concat!(
        r#"async sparePrewarmSupport\(\)\{(?:if\(!(?P<flags>[\w$]+(?:\.[\w$]+)?)\("4237076858"\)(?:\|\|[\w$]+(?:\.[\w$]+)?\(\))?\)"#,
        r#"return\{unsupported:"gate_off"\};|if\(![\w$]+(?:\.[\w$]+)?\(\)\)return\{unsupported:"flags_not_loaded"\};"#,
        r#"if\([\w$]+(?:\.[\w$]+)?\(\)\)return\{unsupported:"connector_names"\};)"#,
        r#"(?P<gate>if\((?P<deploy>[\w$]+(?:\.[\w$]+)?)\(\)\.type!=="1p"\)return\{unsupported:"third_party"\};)?"#,
    )
);

pub fn prewarm_uses_rollout(text: &str) -> Result<bool> {
    Ok(only(prewarm_entry(), text, &[], "原生预热入口")?.name("flags").is_some())
}

rule!(
    prewarm_delegate,
    concat!(
        r"^let (?P<result>[\w$]+)=await (?P<module>[\w$]+)\.backgroundStartSupport\(\);",
        r"return (?P<r2>[\w$]+)\.refused\?\{unsupported:(?P<r3>[\w$]+)\.refused\}:",
        r"\{unsupported:void 0,orgLauncher:(?P<r4>[\w$]+)\.orgLauncher\}\}"
    )
);

pub fn prewarm_helper(text: &str) -> Result<Option<String>> {
    let entry = only(prewarm_entry(), text, &[], "原生预热入口")?;
    let span = entry.get(0).context("预热入口范围缺失")?;
    let end = body_end(text, span.start() + "async sparePrewarmSupport()".len())?;
    let tail = &text[span.end()..end];
    if !tail.contains("backgroundStartSupport") {
        return Ok(None);
    }
    ensure!(entry.name("gate").is_none(), "预热门槛同时出现在本地和共享模块");
    let call = only(prewarm_delegate(), tail, &[&["result", "r2", "r3", "r4"]], "预热共享调用")?;
    module_file(text, &call["module"]).map(Some)
}

fn module_file(text: &str, module: &str) -> Result<String> {
    let binding = Regex::new(&format!(
        r#"(?:\b(?:var|let|const) |,){}=require\("\./(?P<file>index[\w.-]*\.js)"\)"#,
        regex::escape(module)
    ))?;
    let source = only(&binding, text, &[], "预热共享模块引用")?;
    Ok(source["file"].to_owned())
}

rule!(
    prewarm_claim_call,
    concat!(
        r"options:(?P<module>[\w$]+)\.[\w$]+\((?P<args>[\w$]+)\.sdkOptions,",
        r"\{userAdditionalDirectories:(?P<a2>[\w$]+)\.userAdditionalDirectories,bootModel:[\w$]+\.bootModel\}\)"
    )
);

pub fn prewarm_claim_helper(text: &str) -> Result<Option<String>> {
    if prewarm_helper(text)?.is_none() { return Ok(None); }
    let call = only(prewarm_claim_call(), text, &[&["args", "a2"]], "预热接管参数调用")?;
    module_file(text, &call["module"]).map(Some)
}

rule!(
    prewarm_claim_env,
    concat!(
        r"for\(let (?P<key>[\w$]+) of (?P<allowed>[\w$]+)\)\{let (?P<value>[\w$]+)=(?P<options>[\w$]+)\.env\?\.\[(?P<k2>[\w$]+)\];",
        r#"typeof (?P<v2>[\w$]+)=="string"(?P<skip>&&\([^;]{1,400}\))?&&\((?P<env>[\w$]+)\[(?P<k3>[\w$]+)\]=(?P<v3>[\w$]+)\)\}return\{cwd:(?P<o2>[\w$]+)\.cwd"#
    )
);

fn claim_env_parts(text: &str) -> Result<Captures<'_>> {
    let caps = only(prewarm_claim_env(), text, &[&["key", "k2", "k3"], &["value", "v2", "v3"], &["options", "o2"]], "预热接管环境")?;
    let list = format!(r#"{}=["CLAUDE_CODE_OAUTH_TOKEN","CLAUDE_CODE_SESSION_ACCESS_TOKEN","CLAUDE_CODE_HOST_SESSION_ID"]"#, &caps["allowed"]);
    ensure!(text.matches(&list).count() == 1, "预热接管环境白名单已变化");
    Ok(caps)
}

fn claim_env_filter(caps: &Captures<'_>) -> String {
    let options = &caps["options"];
    format!(
        r#"&&({}!=="CLAUDE_CODE_OAUTH_TOKEN"||{}!==""||{options}.env?.CLAUDE_CODE_ENTRYPOINT!=="claude-desktop-3p"||!{options}.env?.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST||!{options}.env?.CLAUDE_CODE_HOST_AUTH_ENV_VAR)"#,
        &caps["key"], &caps["value"]
    )
}

pub fn prewarm_claim_installed(text: &str) -> Result<bool> {
    let caps = claim_env_parts(text)?;
    if let Some(skip) = caps.name("skip") {
        ensure!(skip.as_str() == claim_env_filter(&caps), "预热接管环境补丁引用不一致");
        Ok(true)
    } else { Ok(false) }
}

pub fn patch_prewarm_claim(text: &str) -> Result<String> {
    ensure!(!prewarm_claim_installed(text)?, "预热接管环境已适配");
    let caps = claim_env_parts(text)?;
    let at = caps.name("v2").context("预热接管环境范围缺失")?.end() + "==\"string\"".len();
    Ok(splice(text, &mut vec![(at, at, claim_env_filter(&caps))]))
}

rule!(
    background_start,
    concat!(
        r"async function (?P<name>[\w$]+)\(\)\{",
        r#"(?P<gate>if\([\w$]+(?:\.[\w$]+)?\(\)\.type!=="1p"\)return\{refused:"third_party"\};)?"#,
        r"let (?P<policy>[\w$]+);try\{(?P<p2>[\w$]+)=await (?P<module>[\w$]+)\.[\w$]+\(\)\}",
        r#"catch\{return\{refused:"org_launcher"\}\}return (?P<m2>[\w$]+)\.[\w$]+\((?P<p3>[\w$]+)\)\?"#,
        r#"\{refused:"org_launcher"\}:\{refused:void 0,orgLauncher:(?P<p4>[\w$]+)\}\}"#,
        r"exports\.backgroundStartSupport=(?P<export>[\w$]+);"
    )
);

fn prewarm_gate(text: &str) -> Result<Captures<'_>> {
    if text.contains("exports.backgroundStartSupport=") {
        return only(background_start(), text, &[&["name", "export"], &["policy", "p2", "p3", "p4"], &["module", "m2"]], "预热共享检查");
    }
    ensure!(prewarm_helper(text)?.is_none(), "预热检查位于共享模块，不能按本地入口判断");
    only(prewarm_entry(), text, &[], "原生预热入口")
}

pub fn prewarm_installed(text: &str) -> Result<bool> {
    let caps = prewarm_gate(text)?;
    Ok(caps.name("gate").is_none())
}

pub fn patch_prewarm(text: &str) -> Result<String> {
    let caps = prewarm_gate(text)?;
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
        r"thinking:[\w$]+\((?P<surface>[\w$]+),(?P<model2>[\w$]+)\.id,(?P<settings>[\w$]+)\),",
        r#"(?P<added>\.\.\.(?P<surface2>[\w$]+)==="code"&&(?P<srv1>[\w$]+)\?\.fast_mode!==void 0&&"#,
        r"\{fast_mode:(?P<srv2>[\w$]+)\.fast_mode\},)?",
        r#"(?P<fallback>\.\.\.[\w$]+==="code"&&[\w$]+\?\.fast_mode===void 0&&\["anthropic","gateway"\][^{};]{0,200}\{fast_mode:[^{};]{0,700}\},)?"#,
        r#"(?P<tail>\.\.\.[\w$]+!==void 0&&\{section:"overflow")"#,
    )
);

rule!(
    fast_emit_conditional,
    concat!(
        r"(?P<thinking>[\w$]+)=[\w$]+\((?P<surface>[\w$]+),(?P<model2>[\w$]+)\.id,(?P<settings>[\w$]+)\),",
        r"(?P<g>[\w$]+)=\{id:(?P<model>[\w$]+)\.id,name:[\w$]+,description:[\w$]+,",
        r"thinking:[\w$]+\?[\w$]+\((?P<thinking2>[\w$]+)\):(?P<thinking3>[\w$]+),",
        r#"(?P<added>\.\.\.(?P<surface2>[\w$]+)==="code"&&(?P<srv1>[\w$]+)\?\.fast_mode!==void 0&&"#,
        r"\{fast_mode:(?P<srv2>[\w$]+)\.fast_mode\},)?",
        r#"(?P<fallback>\.\.\.[\w$]+==="code"&&[\w$]+\?\.fast_mode===void 0&&\["anthropic","gateway"\][^{};]{0,200}\{fast_mode:[^{};]{0,700}\},)?"#,
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

rule!(
    fast_catalog,
    concat!(
        r#"var (?P<name>[\w$]+)=\{\$schema:"https://downloads\.claude\.ai/model-catalog/v1/schema\.json",schema_version:1,"#,
        r#"[^{};]{0,400}surfaces:JSON\.parse\('"#,
    )
);

fn fast_catalog_name(text: &str) -> Result<Option<String>> {
    let hits: Vec<_> = fast_catalog().captures_iter(text).collect();
    ensure!(hits.len() <= 1, "官方模型目录不唯一");
    let Some(caps) = hits.first() else { return Ok(None) };
    let start = caps.get(0).context("官方模型目录范围缺失")?.end();
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'\'' {
        end += if bytes[end] == b'\\' { 2 } else { 1 };
    }
    ensure!(end < bytes.len(), "官方模型目录字符串未闭合");
    let raw = text[start..end].replace("\\'", "'");
    let catalog: serde_json::Value = serde_json::from_str(&raw).context("官方模型目录格式已变化")?;
    let configs = catalog.pointer("/ccd/model_selector_config")
        .and_then(serde_json::Value::as_array).context("官方 Desktop 模型目录缺失")?;
    let rows: Vec<_> = configs.iter().filter(|row| row["id"] == "ccd").collect();
    ensure!(rows.len() == 1, "官方 Desktop 模型目录不唯一");
    let models = rows[0]["models"].as_array().context("官方模型列表缺失")?;
    ensure!(models.iter().any(|model| model.pointer("/fast_mode/type").and_then(serde_json::Value::as_str) == Some("toggle")), "官方目录没有 Fast 控件定义");
    Ok(Some(caps["name"].to_owned()))
}

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

fn fast_catalog_fallback(text: &str, parts: &FastParts<'_>) -> Result<String> {
    let Some(catalog) = fast_catalog_name(text)? else { return Ok(String::new()) };
    let row = free_name(&parts.emit["model"], "")?;
    Ok(format!(
        concat!(
            "...{}===\"code\"&&{}?.fast_mode===void 0&&[\"anthropic\",\"gateway\"].includes({}.provider)&&{{fast_mode:",
            "{}.surfaces.ccd.model_selector_config.find({row}=>{row}.id===\"ccd\")?.models.find({row}=>{row}.id===",
            r#"{}.id.replace(/^anthropic\//i,"").replace(/\[1m\]$/i,"").replace(/(\d)\.(\d)/g,"$1-$2"))?.fast_mode}},"#,
        ),
        &parts.emit["surface"], parts.server, &parts.emit["settings"], catalog, &parts.emit["model"],
        row = row,
    ))
}

fn fast_parts<'t>(text: &'t str) -> Result<FastParts<'t>> {
    let parse = only(fast_parse(), text, &[], "Fast 能力解析入口")?;
    let emit = match (fast_emit().is_match(text), fast_emit_conditional().is_match(text)) {
        (true, false) => only(fast_emit(), text, EMIT_ALIASES, "Fast 模型条目入口")?,
        (false, true) => only(
            fast_emit_conditional(),
            text,
            &[&["model", "model2"], &["thinking", "thinking2", "thinking3"]],
            "Fast 模型条目入口",
        )?,
        (false, false) => anyhow::bail!("Fast 模型条目入口未命中"),
        (true, true) => anyhow::bail!("Fast 模型条目入口命中不唯一"),
    };
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
    if let Some(fallback) = parts.emit.name("fallback") {
        ensure!(flags[0] && fallback.as_str() == fast_catalog_fallback(text, &parts)?, "Fast 官方目录映射不一致");
    }
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

pub fn fast_main_upgrade_needed(text: &str) -> Result<bool> {
    Ok(fast_main_installed(text)? == Some(true)
        && fast_catalog_name(text)?.is_some()
        && fast_parts(text)?.emit.name("fallback").is_none())
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
                "...{}===\"code\"&&{}?.fast_mode!==void 0&&{{fast_mode:{}.fast_mode}},{fallback}",
                &parts.emit["surface"], parts.server, parts.server,
                fallback = fast_catalog_fallback(text, &parts)?
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

rule!(
    fast_model_lookup,
    concat!(
        r"fastModeFor:(?P<arg>[\w$]+)=>\{let (?P<value>[\w$]+)=",
        r"(?P<expr>(?P<lookup>[\w$]+)\((?P<input>[\w$]+)\)|",
        r#"\((?P<lookup2>[\w$]+)\((?P<input2>[\w$]+)\)\?\?(?P<lookup3>[\w$]+)\((?P<input3>[\w$]+)\+"\[1m\]"\)\))"#,
        r#"\?\.fast_mode;return (?P<value2>[\w$]+)\?\.type==="toggle""#,
    )
);

fn fast_lookup_parts(text: &str) -> Result<Captures<'_>> {
    let caps = only(fast_model_lookup(), text, &[&["value", "value2"]], "Fast 模型查找入口")?;
    if caps.name("lookup").is_some() {
        ensure!(caps["arg"] == caps["input"], "Fast 模型查找参数不一致");
    } else {
        ensure!(caps["arg"] == caps["input2"] && caps["arg"] == caps["input3"]
            && caps["lookup2"] == caps["lookup3"], "Fast 模型查找参数不一致");
    }
    Ok(caps)
}

pub fn fast_model_lookup_installed(text: &str) -> Result<bool> {
    Ok(fast_lookup_parts(text)?.name("lookup2").is_some())
}

pub fn patch_fast_model_lookup(text: &str) -> Result<String> {
    let caps = fast_lookup_parts(text)?;
    let lookup = caps.name("lookup").context("Fast 模型查找已适配")?.as_str();
    let arg = &caps["arg"];
    let span = caps.name("expr").context("Fast 模型查找范围缺失")?;
    let mut edits = vec![(span.start(), span.end(), format!("({lookup}({arg})??{lookup}({arg}+\"[1m]\"))"))];
    Ok(splice(text, &mut edits))
}

rule!(
    fast_start,
    concat!(
        r"spawnSessionQuery\([\w$]+,[\w$]+,(?P<options>[\w$]+),(?P<flags>[\w$]+),",
        r"[\w$]+,[\w$]+=\[\],[\w$]+=\[\],[\w$]+=\[\],[\w$]+,[\w$]+,[\w$]+\)\{",
        r"(?P<added>typeof [^;]{1,400};)?using "
    )
);

fn fast_start_settings(caps: &Captures<'_>) -> String {
    let options = &caps["options"];
    let flags = &caps["flags"];
    format!(
        "typeof {options}.settings===\"object\"&&typeof {options}.settings?.fastMode===\"boolean\"&&({flags}={{...{flags},fastMode:{options}.settings.fastMode}});"
    )
}

pub fn fast_start_installed(text: &str) -> Result<Option<bool>> {
    if !text.contains("spawnSessionQuery(") {
        return Ok(None);
    }
    let caps = only(fast_start(), text, &[], "Fast 启动设置入口")?;
    if let Some(added) = caps.name("added") {
        ensure!(added.as_str() == fast_start_settings(&caps), "Fast 启动设置引用不一致");
        Ok(Some(true))
    } else {
        Ok(Some(false))
    }
}

pub fn patch_fast_start(text: &str) -> Result<String> {
    ensure!(fast_start_installed(text)? == Some(false), "Fast 启动设置入口不可安装");
    let caps = only(fast_start(), text, &[], "Fast 启动设置入口")?;
    let at = caps.get(0).context("Fast 启动设置范围缺失")?.end() - "using ".len();
    Ok(splice(text, &mut vec![(at, at, fast_start_settings(&caps))]))
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

rule!(
    locale_validator,
    r"function [\w$]+\((?P<arg>[\w$]+)\)\{return (?P<list>[\w$]+)\.includes\((?P<arg2>[\w$]+)\)\}"
);

rule!(
    locale_exact,
    r"function [\w$]+\((?P<arg>[\w$]+)\)\{return (?P<arg2>[\w$]+)&&(?P<list>[\w$]+)\.includes\((?P<arg3>[\w$]+)\)\?(?P<arg4>[\w$]+):void 0\}"
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
    fallback: Option<((String, String), (usize, usize))>,
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
            caps["list"].to_string(),
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
    ensure!(chinese_aware.len() == 1, "语言解析入口不唯一");
    let (resolver, _, _, list_name, resolver_list, _, _) = chinese_aware.remove(0);
    let mut lists = vec![resolver_list];
    let fallback = if plain.len() == 1 {
        let (_, arg_a, arg_b, _, fallback_list, open, end) = plain.remove(0);
        if fallback_list != resolver_list {
            lists.push(fallback_list);
        }
        Some(((arg_a, arg_b), (open, end)))
    } else {
        ensure!(plain.is_empty(), "语言解析入口不唯一");
        let validator: Vec<_> = locale_validator()
            .captures_iter(text)
            .filter(|caps| caps["list"] == list_name && caps["arg"] == caps["arg2"])
            .collect();
        let exact: Vec<_> = locale_exact()
            .captures_iter(text)
            .filter(|caps| {
                caps["list"] == list_name
                    && caps["arg"] == caps["arg2"]
                    && caps["arg"] == caps["arg3"]
                    && caps["arg"] == caps["arg4"]
            })
            .collect();
        ensure!(validator.len() == 1 && exact.len() == 1, "语言白名单调用路径不唯一");
        None
    };
    let patched_lists = lists
        .iter()
        .filter(|(start, end)| text[*start..*end].ends_with(&CHINESE[1..]))
        .count();
    let patched_body = fallback.as_ref().is_none_or(|((arg_a, arg_b), (open, end))| {
        text[*open..*end] == format!("return {resolver}({arg_a},{arg_b})")
    });
    ensure!(
        patched_lists == lists.len() && patched_body
            || patched_lists == 0 && (fallback.is_none() || !patched_body),
        "语言补丁只应用了一部分"
    );
    ensure!(
        patched_lists > 0 || !lists.iter().any(|(s, e)| text[*s..*e].contains("\"zh-Han")),
        "语言白名单已包含中文但缺少解析改动"
    );
    let patched = patched_lists == lists.len();
    Ok(Locale {
        resolver,
        fallback,
        lists,
        patched,
    })
}

pub fn language_installed(text: &str) -> Result<bool> {
    Ok(locale(text)?.patched)
}

pub fn patch_language(text: &str) -> Result<String> {
    let locale = locale(text)?;
    ensure!(!locale.patched, "语言补丁已安装");
    let mut edits = Vec::new();
    if let Some(((arg_a, arg_b), (open, end))) = locale.fallback {
        edits.push((open, end, format!("return {}({arg_a},{arg_b})", locale.resolver)));
    }
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

#[cfg(test)]
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
    /// Windows 主进程读取各开关的位置，规则要求被注入的开关确实有人读
    const GATES: &str = r#"Ky("1291166712",a);Ky("40173473",a);Zy("4272007212",!1);Zy("2784298314",!1);Ky("1346958739",a);Zy("3577536076",!1);Zy("1403324732",!1);"#;

    fn source() -> String {
        format!("var A=1,B=2,C=3,D=4;{GATES}{HELPER}{CONFIG}")
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
        assert_ne!(Platform::Windows.supported() & SIMULATORS, 0);
        assert_ne!(Platform::Macos.supported() & SIMULATORS, 0);
        let android = patch_config(&source(), Platform::Windows, SIMULATORS).unwrap();
        assert!(android.contains("return{...(e.selfHostedSessions?e.builtinBrowserEnabled?A:B:e.builtinBrowserEnabled?C:D),4272007212:fv(!0),2784298314:fv(!0)}"));
    }

    #[test]
    fn config_rejects_unparsed_expressions_and_duplicate_fields() {
        for platform in [Platform::Macos, Platform::Windows] {
            let patched = patch_config(&source(), platform, PREWARM).unwrap();
            for fields in ["4237076858:fv(!0),extra:unknown()", "4237076858:fv(!0),4237076858:fv(!0)"] {
                assert!(config_mask(&patched.replace("4237076858:fv(!0)", fields), platform).is_err());
            }
            let unread = source().replace("\"1291166712\"", "\"removed\"");
            assert!(patch_config(&unread, platform, COMPUTER_USE).is_err());
        }
    }

    #[test]
    fn windows_refuses_switches_the_main_process_never_reads() {
        for (id, mask) in [("4272007212", SIMULATORS), ("2784298314", SIMULATORS), ("40173473", COMPUTER_USE)] {
            let unread = source().replace(&format!("\"{id}\""), "\"0\"");
            assert!(patch_config(&unread, Platform::Windows, mask).is_err(), "{id}");
            assert!(patch_config(&unread, Platform::Macos, mask).is_ok(), "{id}");
        }
        let prewarm_only = source().replace(GATES, "");
        assert!(patch_config(&prewarm_only, Platform::Windows, PREWARM).is_ok());
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

    #[test]
    fn prewarm_keeps_the_additional_native_gate() {
        let source = PREWARM_SOURCE.replace(
            r#"if(!t.x0("4237076858"))"#,
            r#"if(!t.x0("4237076858")||t._n())"#,
        );
        let patched = patch_prewarm(&source).unwrap();
        assert!(prewarm_installed(&patched).unwrap());
        assert!(patched.contains(r#"if(!t.x0("4237076858")||t._n())"#));
        assert!(!patched.contains("third_party"));
    }

    #[test]
    fn prewarm_without_rollout_preserves_readiness_and_launcher_checks() {
        let source = concat!(
            r#"async sparePrewarmSupport(){if(!t.ready())return{unsupported:"flags_not_loaded"};"#,
            r#"if(t.connectors())return{unsupported:"connector_names"};"#,
            r#"if(t.deployment().type!=="1p")return{unsupported:"third_party"};"#,
            r#"let e;try{e=await launcher.read()}catch{return{unsupported:"org_launcher"}}"#,
            r#"return launcher.blocked(e)?{unsupported:"org_launcher"}:{unsupported:void 0,orgLauncher:e}}"#,
        );
        assert!(!prewarm_uses_rollout(source).unwrap());
        assert!(prewarm_uses_rollout(PREWARM_SOURCE).unwrap());
        let patched = patch_prewarm(source).unwrap();
        assert_eq!(patched, source.replace(
            r#"if(t.deployment().type!=="1p")return{unsupported:"third_party"};"#, "",
        ));
        assert!(prewarm_installed(&patched).unwrap());
        assert!(!prewarm_uses_rollout(&patched).unwrap());
        assert!(patch_prewarm(&patched).is_err());
        assert!(prewarm_installed(&format!("{source}{source}")).is_err());
        for marker in ["flags_not_loaded", "connector_names"] {
            assert!(patch_prewarm(&source.replace(marker, "unknown_gate")).is_err());
        }
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
    fn delegated_prewarm_requires_the_referenced_module() {
        let source = concat!(
            r#"const bg=require("./index.helper.js");"#,
            r#"async sparePrewarmSupport(){if(!t.ready())return{unsupported:"flags_not_loaded"};"#,
            r#"if(t.connectors())return{unsupported:"connector_names"};"#,
            "let result=await bg.backgroundStartSupport();return result.refused?",
            "{unsupported:result.refused}:{unsupported:void 0,orgLauncher:result.orgLauncher}}"
        );
        assert_eq!(prewarm_helper(source).unwrap().as_deref(), Some("index.helper.js"));
        assert!(!prewarm_uses_rollout(source).unwrap());
        assert!(prewarm_installed(source).is_err());
        assert!(patch_prewarm(source).is_err());
        for changed in [
            source.replace("index.helper.js", "../other.js"),
            source.replace("orgLauncher:result", "orgLauncher:other"),
            source.replace("bg.backgroundStartSupport()", "other.backgroundStartSupport()"),
            format!(r#"{source}const bg=require("./index.second.js");"#),
        ] {
            assert!(prewarm_helper(&changed).is_err());
        }
    }

    #[test]
    fn prewarm_claim_omits_only_the_empty_host_managed_third_party_token() {
        let source = concat!(
            r#"const allowed=["CLAUDE_CODE_OAUTH_TOKEN","CLAUDE_CODE_SESSION_ACCESS_TOKEN","CLAUDE_CODE_HOST_SESSION_ID"];"#,
            "function claim(e){let i={};for(let t of allowed){let n=e.env?.[t];",
            r#"typeof n=="string"&&(i[t]=n)}return{cwd:e.cwd,env:i}}"#
        );
        assert!(!prewarm_claim_installed(source).unwrap());
        let patched = patch_prewarm_claim(source).unwrap();
        assert!(prewarm_claim_installed(&patched).unwrap());
        assert!(patch_prewarm_claim(&patched).is_err());
        assert!(prewarm_claim_installed(&patched.replace("e.env?.CLAUDE_CODE_ENTRYPOINT", "other.env?.CLAUDE_CODE_ENTRYPOINT")).is_err());
        let script = format!(r#"
            const assert=require('node:assert/strict');{patched}
            const env={{CLAUDE_CODE_OAUTH_TOKEN:'',CLAUDE_CODE_ENTRYPOINT:'claude-desktop-3p',CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST:'1',CLAUDE_CODE_HOST_AUTH_ENV_VAR:'ANTHROPIC_AUTH_TOKEN',CLAUDE_CODE_HOST_SESSION_ID:'session',CLAUDE_CODE_SESSION_ACCESS_TOKEN:'access',ANTHROPIC_AUTH_TOKEN:'provider'}};
            const run=change=>claim({{cwd:'/test',env:{{...env,...change}}}}).env;
            assert.deepEqual(run({{}}),{{CLAUDE_CODE_HOST_SESSION_ID:'session',CLAUDE_CODE_SESSION_ACCESS_TOKEN:'access'}});
            assert.equal(env.CLAUDE_CODE_OAUTH_TOKEN,'');
            for(const change of [{{CLAUDE_CODE_ENTRYPOINT:'claude-desktop'}},{{CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST:''}},{{CLAUDE_CODE_HOST_AUTH_ENV_VAR:undefined}}])assert.equal(run(change).CLAUDE_CODE_OAUTH_TOKEN,'');
            assert.equal(run({{CLAUDE_CODE_OAUTH_TOKEN:'real-token'}}).CLAUDE_CODE_OAUTH_TOKEN,'real-token');
            assert.deepEqual(claim({{cwd:'/test'}}).env,{{}});
        "#);
        let output = std::process::Command::new("node").args(["-e", &script]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn shared_prewarm_preserves_launcher_policy_and_failures() {
        let source = concat!(
            r#"async function start(){if(deployment().type!=="1p")return{refused:"third_party"};"#,
            r#"let policy;try{policy=await launcher.read()}catch{return{refused:"org_launcher"}}"#,
            r#"return launcher.blocked(policy)?{refused:"org_launcher"}:{refused:void 0,orgLauncher:policy}}"#,
            "exports.backgroundStartSupport=start;"
        );
        assert!(!prewarm_installed(source).unwrap());
        let patched = patch_prewarm(source).unwrap();
        assert!(prewarm_installed(&patched).unwrap());
        assert!(patch_prewarm(&patched).is_err());
        assert!(prewarm_installed(&patched.replace("Support=start", "Support=other")).is_err());
        assert!(prewarm_installed(&source.replace("launcher.blocked", "other.blocked")).is_err());
        let script = format!(r#"
            const assert=require('node:assert/strict');
            async function run(source,type,blocked,throws){{
                const exports={{}},policy={{id:'policy'}},calls=[];
                new Function('exports','deployment','launcher',source)(exports,()=>({{type}}),{{
                    read:async()=>{{calls.push('read');if(throws)throw Error('unavailable');return policy}},
                    blocked:value=>{{assert.strictEqual(value,policy);calls.push('check');return blocked}}
                }});
                return {{result:await exports.backgroundStartSupport(),calls,policy}};
            }}
            (async()=>{{
                const original={original},patched={patched};
                const denied=await run(original,'3p',false,false);
                assert.equal(denied.result.refused,'third_party');assert.deepEqual(denied.calls,[]);
                for(const type of ['1p','3p']){{
                    const allowed=await run(patched,type,false,false);
                    assert.strictEqual(allowed.result.orgLauncher,allowed.policy);
                    assert.equal(allowed.result.refused,undefined);
                    assert.deepEqual(allowed.calls,['read','check']);
                    assert.equal((await run(patched,type,true,false)).result.refused,'org_launcher');
                    assert.equal((await run(patched,type,false,true)).result.refused,'org_launcher');
                }}
            }})().catch(error=>{{console.error(error);process.exitCode=1}});
        "#, original=serde_json::to_string(source).unwrap(),patched=serde_json::to_string(&patched).unwrap());
        let output = std::process::Command::new("node").args(["-e", &script]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
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

    #[test]
    fn fast_mapping_accepts_conditional_thinking_from_the_same_model() {
        let source = fast_source().replace(
            "g={id:i.id,name:f,description:h,thinking:aE(t,i.id,e),",
            "q=aE(t,i.id,e),g={id:i.id,name:f,description:h,thinking:r?f(q):q,",
        );
        assert_eq!(fast_main_installed(&source).unwrap(), Some(false));
        let patched = patch_fast_main(&source).unwrap();
        assert_eq!(fast_main_installed(&patched).unwrap(), Some(true));
        assert!(patched.contains(r#"...t==="code"&&s?.fast_mode!==void 0&&{fast_mode:s.fast_mode},"#));
        assert!(fast_main_installed(&source.replace("thinking:r?f(q):q", "thinking:r?f(other):q")).is_err());
    }

    #[test]
    fn fast_start_forwards_choices_before_the_session_exists_without_replacing_other_settings() {
        let source = "spawnSessionQuery(e,n,r,i,a,o=[],s=[],c=[],l,u,d){using f=span();return i}";
        assert_eq!(fast_start_installed(source).unwrap(), Some(false));
        let patched = patch_fast_start(source).unwrap();
        assert_eq!(fast_start_installed(&patched).unwrap(), Some(true));
        assert!(patch_fast_start(&patched).is_err());
        assert!(fast_start_installed(&patched.replace("fastMode:r.settings.fastMode", "fastMode:other.settings.fastMode")).is_err());
        assert!(fast_start_installed(&format!("{source}{source}")).is_err());
        let method = patched.replace("using f=span();", "");
        let script = format!(r#"
            const assert=require('node:assert/strict'),manager={{{method}}};
            const initial={{permissions:{{ask:['Write']}},env:{{EXISTING:'1'}}}};
            const run=settings=>manager.spawnSessionQuery(null,null,{{settings}},initial,null,[],[],[],null,null,{{session:undefined}});
            for (const fastMode of [true,false]){{
                const result=run({{fastMode}});
                assert.equal(result.fastMode,fastMode);
                assert.strictEqual(result.permissions,initial.permissions);
                assert.strictEqual(result.env,initial.env);
            }}
            for (const settings of [undefined,null,'settings.json',{{}},{{fastMode:'true'}}])assert.strictEqual(run(settings),initial);
            assert.equal(initial.fastMode,undefined);
        "#);
        let result = std::process::Command::new("node").args(["-e", &script]).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }

    fn catalog_source() -> String {
        let fast = serde_json::json!({"type":"toggle","name":"Fast mode","options":[{"id":"fast"},{"id":"off"}]});
        let catalog = serde_json::json!({"ccd":{"model_selector_config":[{"id":"ccd","models":[
            {"id":"claude-opus-5-5","fast_mode":fast},
            {"id":"claude-fable-5-2","fast_mode":fast},
            {"id":"claude-sonnet-5-5"}
        ]}]}});
        format!(r#"var baked={{$schema:"https://downloads.claude.ai/model-catalog/v1/schema.json",schema_version:1,surfaces:JSON.parse('{catalog}')}};"#)
    }

    #[test]
    fn fast_catalog_upgrade_preserves_old_installation_detection() {
        let catalog = catalog_source();
        let original = format!("{catalog}{}", fast_source());
        let legacy = format!("{catalog}{}", patch_fast_main(&fast_source()).unwrap());
        assert!(!fast_main_upgrade_needed(&original).unwrap());
        assert!(fast_main_upgrade_needed(&legacy).unwrap());
        let patched = patch_fast_main(&original).unwrap();
        assert_eq!(fast_main_installed(&patched).unwrap(), Some(true));
        assert!(!fast_main_upgrade_needed(&patched).unwrap());
        assert!(fast_main_installed(&patched.replace("baked.surfaces.ccd", "foreign.surfaces.ccd")).is_err());
        assert!(patch_fast_main(&format!("{catalog}{original}")).is_err());
        assert!(patch_fast_main(&original.replace("model_selector_config", "unknown_config")).is_err());
    }

    #[test]
    fn fast_catalog_reuses_official_controls_for_gateway_model_ids() {
        let catalog = catalog_source();
        let source = format!("{catalog}{}", fast_source());
        let patched = patch_fast_main(&source).unwrap();
        let parts = fast_parts(&patched).unwrap();
        let spread = format!("{}{}", parts.emit.name("added").unwrap().as_str(), parts.emit.name("fallback").unwrap().as_str());
        let script = format!(r#"
            const assert=require('node:assert/strict');
            {catalog}
            const models=baked.surfaces.ccd.model_selector_config[0].models;
            function pick(id,s,provider='gateway',t='code'){{let i={{id}},e={{provider}};return {{{spread}}}.fast_mode;}}
            for (const [id,index] of [['anthropic/claude-opus-5.5',0],['anthropic/claude-fable-5.2[1m]',1],['claude-fable-5-2',1]]){{
                assert.strictEqual(pick(id),models[index].fast_mode);
                assert.strictEqual(pick(id,{{fast_mode:null}}),null);
                const custom={{type:'toggle',name:'custom'}};
                assert.strictEqual(pick(id,{{fast_mode:custom}}),custom);
                assert.equal(pick(id,undefined,'bedrock'),undefined);
                assert.equal(pick(id,undefined,'gateway','chat'),undefined);
            }}
            for (const id of ['anthropic/claude-sonnet-5.5','anthropic/claude-fable-5.1','other/claude-opus-5.5','anthropic/claude-opus-latest'])assert.equal(pick(id),undefined);
        "#);
        let result = std::process::Command::new("node").args(["-e", &script]).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }

    #[test]
    fn fast_model_lookup_handles_catalogs_with_only_long_context_rows() {
        let original = r#"const selector={fastModeFor:e=>{let t=g(e)?.fast_mode;return t?.type==="toggle"&&t.options?.some(e=>!!e.id&&e.id!=="off"&&!!e.name)?t:void 0}};"#;
        let patched = patch_fast_model_lookup(original).unwrap();
        assert!(!fast_model_lookup_installed(original).unwrap());
        assert!(fast_model_lookup_installed(&patched).unwrap());
        assert!(patch_fast_model_lookup(&patched).is_err());
        assert!(fast_model_lookup_installed(&patched.replace("g(e+", "other(e+")).is_err());
        let script = format!(r#"
            const assert=require('node:assert/strict');
            const rows=new Map(),g=e=>rows.get(e)??rows.get(e?.replace(/\[1m\]$/,''));
            {patched}
            const fast={{type:'toggle',options:[{{id:'fast',name:'Fast'}}]}};
            rows.set('anthropic/claude-fable-5.2[1m]',{{fast_mode:fast}});
            assert.strictEqual(selector.fastModeFor('anthropic/claude-fable-5.2'),fast);
            assert.strictEqual(selector.fastModeFor('anthropic/claude-fable-5.2[1m]'),fast);
            assert.equal(selector.fastModeFor('unknown'),undefined);
            assert.equal(selector.fastModeFor(undefined),undefined);
            rows.set('anthropic/claude-fable-5.2',{{fast_mode:null}});
            assert.equal(selector.fastModeFor('anthropic/claude-fable-5.2'),undefined);
        "#);
        let result = std::process::Command::new("node").args(["-e", &script]).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
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
    fn chinese_uses_the_shared_language_list() {
        let source = concat!(
            r#"var px=["en-US","fr-FR"];function mx(e){return px.includes(e)}"#,
            r#"function vx(e){return e&&px.includes(e)?e:void 0}"#,
            r#"function Dx(e,t=px){let n=new Map;for(let t of e)if(t){"#,
            r#"let e=t.toLowerCase();if(e.startsWith("zh-"))return n.get(e)}return "en-US"}"#,
        );
        assert!(!language_installed(source).unwrap());
        let patched = patch_language(source).unwrap();
        assert!(language_installed(&patched).unwrap());
        assert_eq!(patched.matches(r#""zh-Hans","zh-Hant""#).count(), 1);
        assert!(patched.contains("function mx(e){return px.includes(e)}"));
        assert!(patch_language(&patched).is_err());
        assert!(language_installed(&source.replace("function mx(e){return px.includes(e)}", "function mx(e){return other.includes(e)}")).is_err());
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
        let output = std::env::var_os("CLAUDEPP_PATCH_OUTPUT").map(std::path::PathBuf::from);
        if let Some(dir) = &output { std::fs::create_dir_all(dir).unwrap(); }
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
                let patched = patch_language(&text).unwrap();
                assert!(language_installed(&patched).unwrap());
                if let Some(dir) = &output { std::fs::write(dir.join(path.file_name().unwrap()).with_extension("mjs"), patched).unwrap(); }
            }
            if fast_state_parts(&text).is_ok() {
                state += 1;
                let patched = patch_fast_state(&text).unwrap();
                assert!(fast_state_installed(&patched).unwrap());
                if let Some(dir) = &output { std::fs::write(dir.join(path.file_name().unwrap()).with_extension("mjs"), patched).unwrap(); }
            }
        }
        assert_eq!((language, state), (1, 1), "语言与 Fast 状态入口各应唯一");
    }

    #[test]
    #[ignore = "需要官方主进程副本"]
    fn current_rules_round_trip_every_combination() {
        let read = |variable| std::fs::read_to_string(std::env::var_os(variable).expect(variable)).unwrap();
        let main = read("CLAUDEPP_MAIN");
        let prewarm = read("CLAUDEPP_PREWARM");
        let platform = match std::env::var("CLAUDEPP_PLATFORM").as_deref() {
            Ok("macos") => Platform::Macos,
            Ok("windows") => Platform::Windows,
            _ => panic!("缺少有效的 CLAUDEPP_PLATFORM"),
        };
        let output = std::env::var_os("CLAUDEPP_PATCH_OUTPUT").map(std::path::PathBuf::from);
        if let Some(dir) = &output { std::fs::create_dir_all(dir).unwrap(); }
        for mask in 0..=platform.supported() {
            let patched = patch_main(&main, platform, mask).unwrap();
            assert_eq!(main_mask(&patched, platform).unwrap(), mask);
            assert_eq!(patched == main, mask == 0);
            if let Some(dir) = &output { std::fs::write(dir.join(format!("main-{mask}.cjs")), &patched).unwrap(); }
        }
        if let Some(name) = prewarm_claim_helper(&prewarm).unwrap() {
            let path = std::path::PathBuf::from(std::env::var_os("CLAUDEPP_PREWARM").unwrap());
            let source = std::fs::read_to_string(path.parent().unwrap().join(&name)).unwrap();
            assert!(!prewarm_claim_installed(&source).unwrap());
            let patched = patch_prewarm_claim(&source).unwrap();
            assert!(prewarm_claim_installed(&patched).unwrap());
            if let Some(dir) = &output { std::fs::write(dir.join(name).with_extension("cjs"), patched).unwrap(); }
        }
        let (name, gate) = match prewarm_helper(&prewarm).unwrap() {
            Some(name) => {
                let path = std::path::PathBuf::from(std::env::var_os("CLAUDEPP_PREWARM").unwrap());
                let source = std::fs::read_to_string(path.parent().unwrap().join(&name)).unwrap();
                (name, source)
            }
            None => ("prewarm.cjs".into(), prewarm),
        };
        let patched = patch_prewarm(&gate).unwrap();
        assert!(!prewarm_installed(&gate).unwrap());
        assert!(prewarm_installed(&patched).unwrap());
        if let Some(dir) = &output { std::fs::write(dir.join(name).with_extension("cjs"), patched).unwrap(); }
    }

    #[test]
    fn main_patch_combines_config_and_fast_mapping() {
        let source = format!("{}{}", source(), fast_source());
        for mask in [
            0,
            COMPUTER_USE,
            SIMULATORS,
            FAST_STATUS,
            COMPUTER_USE | SIMULATORS | PREWARM | FAST_STATUS,
        ] {
            let patched = patch_main(&source, Platform::Windows, mask).unwrap();
            assert_eq!(main_mask(&patched, Platform::Windows).unwrap(), mask);
            if mask != 0 {
                assert!(patch_main(&patched, Platform::Windows, mask).is_err());
            }
        }
    }
}
