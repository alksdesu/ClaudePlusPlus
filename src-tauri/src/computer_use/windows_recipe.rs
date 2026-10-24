use anyhow::{ensure, Context, Result};
use regex::{Captures, Regex};

use super::asar::{hash, PatchState};

const SIGNATURE: &str = r"function (?P<name>[\w$]+)\((?P<arg>[\w$]+)\)\{";
const CHOICE: &str = concat!(
    r"(?P<expression>(?P<arg2>[\w$]+)\.selfHostedSessions\?(?P<arg3>[\w$]+)\.builtinBrowserEnabled\?",
    r"[\w$]+:[\w$]+:(?P<arg4>[\w$]+)\.builtinBrowserEnabled\?[\w$]+:[\w$]+)",
);
const FEATURES: &str = concat!(
    r",1291166712:(?P<wrapper>[\w$]+)\(\{enabled:!0\}\),",
    r"40173473:(?P<wrapper2>[\w$]+)\(!0\)\}\}",
);
const FORCE_VALUE: &str = concat!(
    r"function (?P<name>[\w$]+)\((?P<arg>[\w$]+)\)\{return\{value:(?P<value>[\w$]+),",
    r#"on:!!(?P<on>[\w$]+),off:!(?P<off>[\w$]+),source:"force",experiment:null,experimentResult:null\}\}"#,
);

pub(super) struct WindowsRecipe {
    pub state: PatchState,
    before: String,
    after: String,
    original_hash: String,
    patched_hash: String,
}

fn aliases(caps: &Captures<'_>, names: &[&str]) -> bool {
    names.windows(2).all(|pair| {
        caps.name(pair[0]).map(|m| m.as_str()) == caps.name(pair[1]).map(|m| m.as_str())
    })
}

impl WindowsRecipe {
    pub fn detect(source: &str) -> Result<Option<Self>> {
        if !source.contains(".selfHostedSessions?") || !source.contains(".builtinBrowserEnabled?") {
            return Ok(None);
        }
        let patterns = [
            [SIGNATURE, "return ", CHOICE, r"\}"].concat(),
            [SIGNATURE, r"return\{\.\.\.\(", CHOICE, r"\)", FEATURES].concat(),
            [
                SIGNATURE,
                r"let (?P<local>[\w$]+)=",
                CHOICE,
                r";return\{\.\.\.(?P<local2>[\w$]+)",
                FEATURES,
            ]
            .concat(),
        ];
        let mut matches = Vec::new();
        for (index, pattern) in patterns.iter().enumerate() {
            for caps in Regex::new(pattern)?.captures_iter(source) {
                if aliases(&caps, &["arg", "arg2", "arg3", "arg4"])
                    && aliases(&caps, &["local", "local2"])
                    && aliases(&caps, &["wrapper", "wrapper2"])
                {
                    matches.push((index, caps));
                }
            }
        }
        if matches.is_empty() {
            return Ok(None);
        }
        ensure!(matches.len() == 1, "Computer Use 配置函数命中不唯一");
        let (shape, caps) = matches.remove(0);
        let wrappers: Vec<_> = Regex::new(FORCE_VALUE)?
            .captures_iter(source)
            .filter(|c| aliases(c, &["arg", "value", "on", "off"]))
            .map(|c| c["name"].to_string())
            .collect();
        ensure!(wrappers.len() == 1, "Computer Use 配置辅助函数未唯一命中");
        let wrapper = &wrappers[0];
        ensure!(
            caps.name("wrapper").is_none_or(|m| m.as_str() == wrapper),
            "Computer Use 补丁引用了不同的配置辅助函数"
        );
        let gate_ids: Vec<_> = Regex::new(r#"([\w$]+)="1291166712""#)?
            .captures_iter(source)
            .map(|c| c[1].to_string())
            .collect();
        ensure!(
            gate_ids.len() == 1
                && source.contains(&format!("({},\"enabled\",", gate_ids[0]))
                && Regex::new(r#"process\.platform==="win32"&&![\w$]+\("40173473"\)"#)?
                    .find_iter(source)
                    .count()
                    == 1,
            "Computer Use 功能门控结构已变化"
        );
        let before = format!(
            "function {}({}){{return {}}}",
            &caps["name"], &caps["arg"], &caps["expression"]
        );
        let after = if shape == 0 {
            format!(
                "function {}({}){{return{{...({}),1291166712:{wrapper}({{enabled:!0}}),40173473:{wrapper}(!0)}}}}",
                &caps["name"], &caps["arg"], &caps["expression"]
            )
        } else {
            caps[0].to_string()
        };
        let span = caps.get(0).context("缺少配置函数范围")?.range();
        let mut original = source.to_string();
        original.replace_range(span.clone(), &before);
        let mut patched = source.to_string();
        patched.replace_range(span, &after);
        Ok(Some(Self {
            state: if shape == 0 {
                PatchState::Pristine
            } else {
                PatchState::Patched
            },
            before,
            after,
            original_hash: hash(original.as_bytes()),
            patched_hash: hash(patched.as_bytes()),
        }))
    }

    pub fn patch(&self, source: &str) -> Result<String> {
        ensure!(
            self.state == PatchState::Pristine
                && hash(source.as_bytes()) == self.original_hash
                && source.matches(&self.before).count() == 1,
            "Windows 主进程内容在检测后发生变化"
        );
        let patched = source.replacen(&self.before, &self.after, 1);
        ensure!(
            hash(patched.as_bytes()) == self.patched_hash,
            "Windows 补丁输出哈希不匹配"
        );
        Ok(patched)
    }
}

#[cfg(test)]
pub(super) const TEST_SOURCE: &str = concat!(
    r#"var A={a:1},B={b:1},C={c:1},D={d:1},gate="1291166712";"#,
    r#"function forceValue(e){return{value:e,on:!!e,off:!e,source:"force",experiment:null,experimentResult:null}}"#,
    r#"function selectFlags(e){return e.selfHostedSessions?e.builtinBrowserEnabled?A:B:e.builtinBrowserEnabled?C:D}"#,
    r#"function enabled(){return process.platform==="win32"&&!flag("40173473")?!1:readFlag(gate,"enabled",false,bool())}"#,
    r#"function consent(){return enabled()&&settings("chicagoEnabled")}"#,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_hashes_come_from_current_source() {
        let renamed = TEST_SOURCE
            .replace("selectFlags", "$renamed$")
            .replace("forceValue", "$helper$")
            .replace("(e)", "($arg$)")
            .replace("e.", "$arg$.")
            .replace("value:e", "value:$arg$")
            .replace("!!e", "!!$arg$")
            .replace("!e,", "!$arg$,");
        for source in [TEST_SOURCE, &renamed] {
            let recipe = WindowsRecipe::detect(source).unwrap().unwrap();
            assert_eq!(recipe.original_hash, hash(source.as_bytes()));
            let patched = recipe.patch(source).unwrap();
            let detected = WindowsRecipe::detect(&patched).unwrap().unwrap();
            assert_eq!(detected.state, PatchState::Patched);
            assert_eq!(detected.original_hash, recipe.original_hash);
            assert_eq!(detected.patched_hash, recipe.patched_hash);
            assert!(detected.patch(&patched).is_err());
            assert!(!recipe.after.contains("chicagoEnabled"));
            assert!(!recipe.after.contains("appScopedEnabled"));
            assert!(recipe.patch(&format!("{source};externalChange()")).is_err());
        }
    }

    #[test]
    fn existing_local_variable_patch_is_recognized() {
        let recipe = WindowsRecipe::detect(TEST_SOURCE).unwrap().unwrap();
        let legacy = TEST_SOURCE.replace(&recipe.before, concat!(
            "function selectFlags(e){let t=e.selfHostedSessions?e.builtinBrowserEnabled?A:B:e.builtinBrowserEnabled?C:D;",
            "return{...t,1291166712:forceValue({enabled:!0}),40173473:forceValue(!0)}}",
        ));
        let detected = WindowsRecipe::detect(&legacy).unwrap().unwrap();
        assert_eq!(detected.state, PatchState::Patched);
        assert_eq!(detected.original_hash, recipe.original_hash);
    }

    #[test]
    fn ambiguous_or_changed_semantics_are_refused() {
        assert!(WindowsRecipe::detect(&format!("{TEST_SOURCE}{TEST_SOURCE}")).is_err());
        for (from, to) in [
            ("on:!!e", "on:!e"),
            ("value:e", "value:other"),
            ("source:\"force\"", "source:\"remote\""),
            ("40173473", "anotherGate"),
            ("readFlag(gate,", "readFlag(other,"),
            ("e.selfHostedSessions", "other.selfHostedSessions"),
        ] {
            assert!(
                matches!(
                    WindowsRecipe::detect(&TEST_SOURCE.replace(from, to)),
                    Err(_) | Ok(None)
                ),
                "{from}"
            );
        }
    }
}
