//! Splits test files into one unit per test, each carrying the helpers it
//! calls, so Jev judges one test at a time instead of a whole file.

use std::collections::{BTreeSet, HashSet, VecDeque};
use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Go,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Lang {
    pub fn for_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next()?;
        Some(match ext {
            "go" => Lang::Go,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            _ => return None,
        })
    }

    fn language(self) -> Language {
        match self {
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Unit {
    pub name: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
    idents: HashSet<String>,
    /// Setup hooks (beforeEach and friends) from the enclosing describe blocks.
    hooks: Vec<Decl>,
}

#[derive(Debug, Clone)]
pub struct Decl {
    pub name: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
    idents: HashSet<String>,
}

/// A parsed source file: its tests and the declarations a test may call.
pub struct Parsed {
    pub units: Vec<Unit>,
    /// Declarations visible to every unit (top level of the file).
    pub decls: Vec<Decl>,
    /// Declarations scoped to describe blocks, keyed by unit index.
    scoped: Vec<Vec<Decl>>,
}

pub fn parse(lang: Lang, path: &str, source: &str) -> Option<Parsed> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let src = source.as_bytes();
    let decls = top_level_decls(lang, root, src, path);
    let (units, scoped) = match lang {
        Lang::Go => (go_tests(root, src), Vec::new()),
        _ => js_tests(root, src, path),
    };
    Some(Parsed {
        units,
        decls,
        scoped,
    })
}

/// Declarations from other files that share a scope with this one, such as
/// Go helpers in sibling `_test.go` files of the same package.
pub fn decls_only(lang: Lang, path: &str, source: &str) -> Vec<Decl> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    top_level_decls(lang, tree.root_node(), source.as_bytes(), path)
}

/// The helpers a unit needs, nearest first: hooks, then every declaration its
/// code reaches through identifiers, transitively.
pub fn helpers_for<'a>(parsed: &'a Parsed, unit_index: usize, extra: &'a [Decl]) -> Vec<&'a Decl> {
    let unit = &parsed.units[unit_index];
    let scoped = parsed
        .scoped
        .get(unit_index)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    // Inner scopes shadow outer ones, and this file shadows sibling files.
    let pool: Vec<&Decl> = scoped.iter().chain(&parsed.decls).chain(extra).collect();

    let mut out: Vec<&Decl> = unit.hooks.iter().collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&HashSet<String>> = VecDeque::from([&unit.idents]);
    queue.extend(unit.hooks.iter().map(|h| &h.idents));
    while let Some(idents) = queue.pop_front() {
        let mut names: Vec<&String> = idents.iter().collect();
        names.sort();
        for name in names {
            if name == &unit.name || !seen.insert(name.as_str()) {
                continue;
            }
            if let Some(decl) = pool.iter().find(|d| &d.name == name) {
                out.push(decl);
                queue.push_back(&decl.idents);
            }
        }
    }
    out
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn idents(node: Node, src: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cursor = node.walk();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if matches!(
            n.kind(),
            "identifier"
                | "field_identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
        ) {
            out.insert(text(n, src).to_string());
        }
        stack.extend(n.children(&mut cursor));
    }
    out
}

fn decl(name: &str, node: Node, src: &[u8], path: &str) -> Decl {
    Decl {
        name: name.to_string(),
        path: path.to_string(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        code: text(node, src).to_string(),
        idents: idents(node, src),
    }
}

fn is_go_test_name(name: &str) -> bool {
    name.strip_prefix("Test")
        .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_lowercase()))
        && name != "TestMain"
}

fn top_level_decls(lang: Lang, root: Node, src: &[u8], path: &str) -> Vec<Decl> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match lang {
            Lang::Go => go_decls(child, src, path, &mut out),
            _ => js_decls(child, src, path, &mut out),
        }
    }
    out
}

fn go_decls(node: Node, src: &[u8], path: &str, out: &mut Vec<Decl>) {
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name").map(|n| text(n, src))
                && !is_go_test_name(name)
                && !name.starts_with("Benchmark")
                && !name.starts_with("Fuzz")
            {
                out.push(decl(name, node, src, path));
            }
        }
        "type_declaration" | "var_declaration" | "const_declaration" => {
            let mut names = BTreeSet::new();
            let mut cursor = node.walk();
            for spec in node.named_children(&mut cursor) {
                let mut c = spec.walk();
                if let Some(n) = spec.child_by_field_name("name") {
                    names.insert(text(n, src).to_string());
                }
                for n in spec.children_by_field_name("name", &mut c) {
                    names.insert(text(n, src).to_string());
                }
            }
            for name in names {
                out.push(decl(&name, node, src, path));
            }
        }
        _ => {}
    }
}

fn go_tests(root: Node, src: &[u8]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "function_declaration" {
            continue;
        }
        let Some(name) = child.child_by_field_name("name").map(|n| text(n, src)) else {
            continue;
        };
        if is_go_test_name(name) {
            out.push(Unit {
                name: name.to_string(),
                start_line: child.start_position().row + 1,
                end_line: child.end_position().row + 1,
                code: text(child, src).to_string(),
                idents: idents(child, src),
                hooks: Vec::new(),
            });
        }
    }
    out
}

fn js_decls(node: Node, src: &[u8], path: &str, out: &mut Vec<Decl>) {
    match node.kind() {
        "export_statement" => {
            if let Some(inner) = node.child_by_field_name("declaration") {
                js_decls(inner, src, path, out);
            }
        }
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(decl(text(name, src), node, src, path));
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            for d in node.named_children(&mut cursor) {
                if let Some(name) = d
                    .child_by_field_name("name")
                    .filter(|n| n.kind() == "identifier")
                {
                    out.push(decl(text(name, src), node, src, path));
                }
            }
        }
        _ => {}
    }
}

const JS_SCOPES: [&str; 5] = [
    "describe",
    "beforeEach",
    "beforeAll",
    "afterEach",
    "afterAll",
];

/// `it`, `test`, `describe` and hooks, with `.only`/`.skip`/`.each` forms.
/// Playwright's `test.describe`/`test.beforeEach` take the property's kind.
fn js_call_kind<'a>(call: Node, src: &'a [u8]) -> Option<&'a str> {
    let mut f = call.child_by_field_name("function")?;
    let mut scope = None;
    loop {
        match f.kind() {
            "identifier" => break,
            "member_expression" => {
                let prop = text(f.child_by_field_name("property")?, src);
                if prop == "step" {
                    return None;
                }
                scope = scope.or(JS_SCOPES.into_iter().find(|s| *s == prop));
                f = f.child_by_field_name("object")?;
            }
            "call_expression" => f = f.child_by_field_name("function")?,
            _ => return None,
        }
    }
    let name = text(f, src);
    if JS_SCOPES.contains(&name) {
        return Some(name);
    }
    matches!(name, "it" | "test").then_some(scope.unwrap_or(name))
}

fn js_title(call: Node, src: &[u8]) -> String {
    let arg = call
        .child_by_field_name("arguments")
        .and_then(|a| a.named_child(0))
        .map(|a| text(a, src).to_string())
        .unwrap_or_default();
    arg.trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_string()
}

fn js_tests(root: Node, src: &[u8], path: &str) -> (Vec<Unit>, Vec<Vec<Decl>>) {
    let mut units = Vec::new();
    let mut scoped = Vec::new();
    let mut stack = vec![root];
    let mut cursor = root.walk();
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && matches!(js_call_kind(node, src), Some("it" | "test"))
        {
            let mut titles = vec![js_title(node, src)];
            let mut hooks = Vec::new();
            let mut decls = Vec::new();
            let mut up = node.parent();
            while let Some(a) = up {
                if a.kind() == "call_expression" && js_call_kind(a, src) == Some("describe") {
                    titles.push(js_title(a, src));
                }
                if a.kind() == "statement_block" {
                    let mut c = a.walk();
                    for stmt in a.named_children(&mut c) {
                        js_decls(stmt, src, path, &mut decls);
                        let call = stmt.named_child(0).filter(|n| {
                            stmt.kind() == "expression_statement" && n.kind() == "call_expression"
                        });
                        if let Some(call) = call
                            && let Some(
                                kind @ ("beforeEach" | "beforeAll" | "afterEach" | "afterAll"),
                            ) = js_call_kind(call, src)
                        {
                            hooks.push(decl(kind, call, src, path));
                        }
                    }
                }
                up = a.parent();
            }
            hooks.reverse();
            titles.reverse();
            units.push(Unit {
                name: titles.join(" > "),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                code: text(node, src).to_string(),
                idents: idents(node, src),
                hooks,
            });
            scoped.push(decls);
            continue;
        }
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    (units, scoped)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO: &str = r#"package q

type harness struct{ q *Queue }

func newHarness(t *testing.T) *harness { return &harness{q: New()} }

func (h *harness) waitDrained() { for h.q.Len() > 0 { time.Sleep(pollEvery) } }

const pollEvery = 10 * time.Millisecond

func unused() {}

func TestMain(m *testing.M) { m.Run() }

func TestDrains(t *testing.T) {
	h := newHarness(t)
	h.waitDrained()
}

func Testlowercase(t *testing.T) {}

func TestOther(t *testing.T) { t.Fatal("x") }
"#;

    fn names(ds: &[&Decl]) -> Vec<String> {
        ds.iter().map(|d| d.name.clone()).collect()
    }

    #[test]
    fn go_tests_carry_their_transitive_helpers() {
        let p = parse(Lang::Go, "q_test.go", GO).unwrap();
        let units: Vec<_> = p
            .units
            .iter()
            .map(|u| (u.name.as_str(), u.start_line))
            .collect();
        assert_eq!(units, [("TestDrains", 15), ("TestOther", 22)]);
        let helpers = names(&helpers_for(&p, 0, &[]));
        assert_eq!(
            helpers,
            ["newHarness", "waitDrained", "harness", "pollEvery"]
        );
        assert!(helpers_for(&p, 1, &[]).is_empty());
    }

    #[test]
    fn go_helpers_resolve_from_sibling_files_but_the_file_wins() {
        let p = parse(
            Lang::Go,
            "a_test.go",
            "package q\nfunc TestA(t *testing.T) { waitFor(t); local() }\nfunc local() {}\n",
        )
        .unwrap();
        let sibling = decls_only(
            Lang::Go,
            "helpers_test.go",
            "package q\nfunc waitFor(t *testing.T) {}\nfunc local() { other() }\n",
        );
        let helpers = helpers_for(&p, 0, &sibling);
        let got: Vec<_> = helpers
            .iter()
            .map(|d| (d.name.as_str(), d.path.as_str()))
            .collect();
        assert_eq!(
            got,
            [("local", "a_test.go"), ("waitFor", "helpers_test.go")]
        );
    }

    const TS: &str = r#"import { render } from './r';

const makeStore = () => new Store();
function unused() {}

describe('queue', () => {
  let store: Store;
  beforeEach(() => { store = makeStore(); });

  describe('drain', () => {
    it.each([1, 2])('drains %d', async (n) => {
      await waitFor(() => expect(store.size).toBe(0));
    });
  });

  test('empty', () => { expect(render()).toBe(''); });
});
"#;

    #[test]
    fn js_tests_are_named_by_describe_path_and_carry_hooks_and_scope() {
        let p = parse(Lang::TypeScript, "q.test.ts", TS).unwrap();
        let units: Vec<_> = p
            .units
            .iter()
            .map(|u| (u.name.as_str(), u.start_line))
            .collect();
        assert_eq!(
            units,
            [("queue > drain > drains %d", 11), ("queue > empty", 16)]
        );
        assert_eq!(
            names(&helpers_for(&p, 0, &[])),
            ["beforeEach", "store", "makeStore"]
        );
        assert_eq!(
            names(&helpers_for(&p, 1, &[])),
            ["beforeEach", "makeStore", "store"]
        );
    }

    #[test]
    fn playwright_describe_hooks_and_steps_are_not_tests() {
        let src = "test.describe('tile', () => {\n  test.beforeEach(async () => { await setup(); });\n  test('opens', async () => {\n    await test.step('click', async () => {});\n  });\n});\nfunction setup() {}\n";
        let p = parse(Lang::TypeScript, "t.spec.ts", src).unwrap();
        let units: Vec<_> = p
            .units
            .iter()
            .map(|u| (u.name.as_str(), u.start_line))
            .collect();
        assert_eq!(units, [("tile > opens", 3)]);
        assert_eq!(names(&helpers_for(&p, 0, &[])), ["beforeEach", "setup"]);
    }

    #[test]
    fn files_without_tests_have_no_units() {
        let p = parse(
            Lang::Go,
            "main_test.go",
            "package q\nfunc TestMain(m *testing.M) { m.Run() }\n",
        )
        .unwrap();
        assert!(p.units.is_empty());
        assert_eq!(Lang::for_path("a/b.test.mjs"), Some(Lang::JavaScript));
        assert_eq!(Lang::for_path("run_test.sh"), None);
    }
}
