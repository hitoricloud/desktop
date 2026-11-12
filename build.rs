use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

/// Same order as `i18n::Lang`; English is the fallback and defines the keys.
const LOCALES: [&str; 2] = ["en", "ru"];
/// Plural forms in the order of `i18n::ONE`..`i18n::OTHER`.
const FORMS: [&str; 4] = ["one", "few", "many", "other"];

type Forms = [Option<String>; 4];

fn main() {
    // Per-monitor DPI awareness, common controls v6 and the app icon for hitori.exe (also when cross-building).
    println!("cargo:rerun-if-changed=packaging/windows");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("packaging/windows/hitori.rc", embed_resource::NONE).manifest_required().unwrap();
    }
    locales();
}

/// `locales/<lang>/<area>.toml` → key tables and the `t!` macro, one arm per English key (unknown keys do not compile).
fn locales() {
    println!("cargo:rerun-if-changed=locales");
    let tables: Vec<BTreeMap<String, Forms>> = LOCALES.iter().map(|l| load(&Path::new("locales").join(l))).collect();
    let en = &tables[0];
    let keys: Vec<&String> = en.keys().collect();
    let args: Vec<Vec<String>> = keys.iter().map(|k| placeholders(&en[*k], k)).collect();
    let mut out = String::new();
    writeln!(out, "pub const KEYS: [&str; {}] = {:?};", keys.len(), keys).unwrap();
    let slices: Vec<String> = args.iter().map(|a| format!("&{a:?}")).collect();
    writeln!(out, "pub const ARGS: [&[&str]; {}] = [{}];", keys.len(), slices.join(", ")).unwrap();
    writeln!(out, "pub static TEXT: [[[Option<&str>; 4]; {}]; {}] = [", keys.len(), LOCALES.len()).unwrap();
    for table in &tables {
        out.push_str("[\n");
        for k in &keys {
            writeln!(out, "{:?},", table.get(*k).cloned().unwrap_or_default()).unwrap();
        }
        out.push_str("],\n");
    }
    out.push_str("];\n");
    let unknown: Vec<String> = LOCALES.iter().zip(&tables).flat_map(|(l, t)| t.keys().filter(|k| !en.contains_key(*k)).map(move |k| format!("{l}: {k}"))).collect();
    writeln!(out, "#[cfg(test)]\npub const UNKNOWN: &[&str] = &{unknown:?};").unwrap();
    out.push_str("macro_rules! t {\n");
    for (i, (k, a)) in keys.iter().zip(&args).enumerate() {
        let plural = en[*k][3].is_some() && en[*k][0].is_some();
        let pattern: Vec<String> = a.iter().enumerate().map(|(j, name)| format!(", {name} = $a{j}:expr")).collect();
        let values: Vec<String> = (0..a.len()).map(|j| format!("&$a{j}")).collect();
        let body = match (plural, a.is_empty()) {
            (true, _) => format!("{{ let n = $a0; $crate::i18n::format({i}, Some(n as u64), &[&n, {}]) }}", values[1..].join(", ")),
            (false, true) => format!("$crate::i18n::text({i})"),
            (false, false) => format!("$crate::i18n::format({i}, None, &[{}])", values.join(", ")),
        };
        writeln!(out, "    ({k:?}{} $(,)?) => {{ {body} }};", pattern.concat()).unwrap();
        let wants: Vec<String> = a.iter().map(|name| format!("{name} = …")).collect();
        writeln!(out, "    ({k:?} $($rest:tt)*) => {{ compile_error!({:?}) }};", format!("t!({k:?}) takes ({})", wants.join(", "))).unwrap();
    }
    out.push_str("    ($k:literal $($rest:tt)*) => { compile_error!(concat!(\"no such key in locales/en: \", $k)) };\n}\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("i18n.rs"), out).unwrap();
}

/// Every message of one language: `<area>.<path>` → forms; a plain string is the `other` form.
fn load(dir: &Path) -> BTreeMap<String, Forms> {
    let mut all = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return all };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let text = std::fs::read_to_string(&path).unwrap();
            let table: toml::Table = text.parse().unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            walk(&path.file_stem().unwrap().to_string_lossy(), &table, &path, &mut all);
        }
    }
    all
}

fn walk(prefix: &str, table: &toml::Table, path: &Path, all: &mut BTreeMap<String, Forms>) {
    for (name, value) in table {
        let key = format!("{prefix}.{name}");
        let forms = match value {
            toml::Value::String(s) => [None, None, None, Some(s.clone())],
            toml::Value::Table(t) if !t.is_empty() && t.keys().all(|f| FORMS.contains(&f.as_str())) => FORMS.map(|f| t.get(f).map(|v| v.as_str().unwrap_or_else(|| panic!("{}: {key}.{f} is not a string", path.display())).to_owned())),
            toml::Value::Table(t) => {
                walk(&key, t, path, all);
                continue;
            }
            _ => panic!("{}: {key} is not a string or a table", path.display()),
        };
        if all.insert(key.clone(), forms).is_some() {
            panic!("{}: {key} is defined twice", path.display());
        }
    }
}

/// `{name}` placeholders in order of first appearance; a plural message takes its count `n` first.
fn placeholders(forms: &Forms, key: &str) -> Vec<String> {
    let plural = forms[0].is_some();
    if plural && forms[3].is_none() {
        panic!("locales/en: {key} has `one` but no `other`");
    }
    let mut names: Vec<String> = if plural { vec!["n".into()] } else { vec![] };
    for text in forms.iter().flatten() {
        let mut rest = text.as_str();
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            if let Some(close) = rest.find('}') {
                let name = &rest[..close];
                if !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') && !names.iter().any(|n| n == name) {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names
}
