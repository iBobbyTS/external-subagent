//! Location and release metadata for the ZCode Desktop Built-in provider cache.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

const DEFAULT_ENDPOINT: &str = "https://zcode.z.ai";
const MAX_SAFE_REVISION: u64 = (1 << 53) - 1;

pub(super) fn resolve(bundled: &Path) -> Option<PathBuf> {
    let config_root = PathBuf::from(env::var_os("HOME")?).join(".zcode/v2");
    let platform = format!("darwin-{}", env::consts::ARCH);
    let endpoint = endpoint_origin(
        |key| env::var(key).ok(),
        || {
            let settings = super::load_config(&config_root.join("setting.json"))
                .ok()
                .flatten()?;
            settings
                .get("zcodeEndpointOrigin")?
                .as_str()
                .map(str::to_owned)
        },
    )?;
    resolve_in(bundled, &config_root, &platform, &endpoint)
}

fn resolve_in(
    bundled: &Path,
    config_root: &Path,
    platform: &str,
    endpoint: &str,
) -> Option<PathBuf> {
    // The cache is scoped by the Desktop version, not `zcode.cjs --version`.
    let contents = bundled
        .ancestors()
        .find(|path| path.ends_with("Contents/Resources"))?
        .parent()?;
    let info = plist::Value::from_file(contents.join("Info.plist")).ok()?;
    let version = info
        .as_dictionary()?
        .get("CFBundleShortVersionString")?
        .as_string()?
        .trim();
    if version.is_empty() || matches!(version, "." | "..") || version.contains(['/', '\\']) {
        return None;
    }

    let endpoint_digest = format!("{:x}", Sha256::digest(endpoint.as_bytes()));
    let cached = config_root
        .join("runtime/provider")
        .join(platform)
        .join(version)
        .join(format!("endpoint-{}", &endpoint_digest[..32]))
        .join("zcode-builtin.json");
    select_newer_cache(bundled, cached)
}

// Mirror Desktop's endpoint precedence. Custom settings are used only in
// test mode, and the cache key hashes the normalized HTTP(S) origin.
fn endpoint_origin(
    read_env: impl Fn(&str) -> Option<String>,
    read_settings_origin: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let in_test =
        read_env("ZCODE_ENV").is_some_and(|value| value.trim().eq_ignore_ascii_case("test"));
    let scoped = if in_test {
        "ZCODE_TEST_BASE_URL"
    } else {
        "ZCODE_PRODUCTION_BASE_URL"
    };
    let configured = ["ZCODE_BASE_URL", "ZCODE_ENDPOINT_ORIGIN", scoped]
        .into_iter()
        .find_map(|key| nonempty(read_env(key)));
    let setting = if in_test {
        nonempty(read_settings_origin())
    } else {
        None
    };
    let url = Url::parse(
        &setting
            .or(configured)
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned()),
    )
    .ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn select_newer_cache(bundled: &Path, cached: PathBuf) -> Option<PathBuf> {
    let (cached_schema, cached_revision) = release_metadata(&cached)?;
    let (bundled_schema, bundled_revision) = release_metadata(bundled)?;
    (cached_schema == bundled_schema && cached_revision > bundled_revision).then_some(cached)
}

fn release_metadata(path: &Path) -> Option<(u64, u64)> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let release = super::load_config(path).ok().flatten()?;
    let schema = release.get("schemaVersion")?.as_u64()?;
    let revision = release.get("revision")?.as_u64()?;
    if revision > MAX_SAFE_REVISION {
        return None;
    }
    // Check the release envelope and rule containers. ZCode owns the rule
    // contents and interprets every model and reasoning option.
    for (group, arrays) in [
        (
            "providerConfigRules",
            &["templateRules", "providerRules"][..],
        ),
        (
            "modelConfigRules",
            &[
                "modelRules",
                "modelApiRules",
                "providerSiteRules",
                "templateModelRules",
                "builtinProviderModelRules",
            ][..],
        ),
    ] {
        let group = release.get("config")?.get(group)?;
        for key in arrays {
            let rules = group.get(key)?.as_array()?;
            if !rules
                .iter()
                .all(|rule| rule.get("config").is_some_and(Value::is_object))
            {
                return None;
            }
        }
    }
    Some((schema, revision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(revision: u64) -> Value {
        json!({
            "schemaVersion": 1,
            "revision": revision,
            "config": {
                "providerConfigRules": { "templateRules": [], "providerRules": [] },
                "modelConfigRules": {
                    "modelRules": [], "modelApiRules": [], "providerSiteRules": [],
                    "templateModelRules": [], "builtinProviderModelRules": []
                }
            }
        })
    }

    #[test]
    fn selects_only_a_newer_release_in_the_installed_app_scope() {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp.path().join("ZCode.app/Contents");
        let bundled = contents.join("Resources/config/provider/zcode-builtin.json");
        fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        fs::write(&bundled, release(44).to_string()).unwrap();
        let plist = contents.join("Info.plist");
        plist::to_file_binary(&plist, &json!({ "CFBundleShortVersionString": "4.0.0" })).unwrap();
        let config_root = temp.path().join(".zcode/v2");
        let cached = config_root.join(concat!(
            "runtime/provider/darwin-aarch64/4.0.0/",
            "endpoint-78d7c3bef4024722642626fe3669a799/zcode-builtin.json"
        ));
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        let resolve = || resolve_in(&bundled, &config_root, "darwin-aarch64", DEFAULT_ENDPOINT);
        assert_eq!(resolve(), None);
        fs::write(&cached, release(48).to_string()).unwrap();
        assert_eq!(resolve(), Some(cached.clone()));
        assert_eq!(
            resolve_in(&bundled, &config_root, "darwin-x86_64", DEFAULT_ENDPOINT),
            None
        );
        assert_eq!(
            resolve_in(
                &bundled,
                &config_root,
                "darwin-aarch64",
                "https://other.example"
            ),
            None
        );
        plist::to_file_xml(&plist, &json!({ "CFBundleShortVersionString": "4.0.1" })).unwrap();
        assert_eq!(resolve(), None);
        plist::to_file_xml(&plist, &json!({ "CFBundleShortVersionString": "4.0.0" })).unwrap();
        let mut wrong_schema = release(48);
        wrong_schema["schemaVersion"] = json!(2);
        let mut bad_rules = release(48);
        bad_rules["config"]["providerConfigRules"]["templateRules"] = json!([null]);
        for unusable in [
            "truncated".to_owned(),
            release(43).to_string(),
            release(44).to_string(),
            wrong_schema.to_string(),
            bad_rules.to_string(),
        ] {
            fs::write(&cached, unusable).unwrap();
            assert_eq!(resolve(), None);
        }
    }

    #[test]
    fn normalizes_endpoint_and_respects_desktop_precedence() {
        let env = |key: &str| match key {
            "ZCODE_ENV" => Some("TEST".to_owned()),
            "ZCODE_BASE_URL" => Some(" https://ENV.example:443/path ".to_owned()),
            _ => None,
        };
        assert_eq!(
            endpoint_origin(env, || Some(
                " https://SETTINGS.example:443/path ".to_owned()
            ))
            .as_deref(),
            Some("https://settings.example")
        );
        assert_eq!(
            endpoint_origin(env, || None).as_deref(),
            Some("https://env.example")
        );
        assert_eq!(
            endpoint_origin(|_| None, || panic!("production ignores settings")).as_deref(),
            Some(DEFAULT_ENDPOINT)
        );
        assert_eq!(
            endpoint_origin(|_| Some("file:///tmp".to_owned()), || None),
            None
        );
    }
}
