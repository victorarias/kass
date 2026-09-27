//! Splits test files into one unit per test, each carrying the helpers it
//! calls, so Jev judges one test at a time instead of a whole file.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
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

/// Code the file's tests don't cover, such as specs from a test library
/// kass doesn't know; judged separately so nothing goes unjudged.
#[derive(Debug, Clone)]
pub struct Snippet {
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
}

/// A parsed source file: its tests and the declarations a test may call.
pub struct Parsed {
    pub units: Vec<Unit>,
    /// Declarations visible to every unit (top level of the file).
    pub decls: Vec<Decl>,
    /// Declarations scoped to describe blocks, keyed by unit index.
    scoped: Vec<Vec<Decl>>,
    /// Statements outside every unit and every helper a unit reaches.
    pub rest: Vec<Snippet>,
}

pub fn parse(lang: Lang, path: &str, source: &str) -> Option<Parsed> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let src = source.as_bytes();
    let decls = top_level_decls(lang, root, src, path);
    let (units, scoped) = match lang {
        Lang::Go => {
            let mut units = go_tests(root, src, path);
            let mut cursor = root.walk();
            let specs = root
                .named_children(&mut cursor)
                .filter(|n| !matches!(n.kind(), "function_declaration" | "method_declaration"));
            let (spec_units, scoped) = spec_tests(lang, &GINKGO, specs, src, path);
            let plain = units.len();
            units.extend(spec_units);
            let scoped = std::iter::repeat_n(Vec::new(), plain)
                .chain(scoped)
                .collect();
            (units, scoped)
        }
        _ => spec_tests(lang, &JS, [root].into_iter(), src, path),
    };
    let mut parsed = Parsed {
        units,
        decls,
        scoped,
        rest: Vec::new(),
    };
    if !parsed.units.is_empty() {
        let mut covered = Vec::new();
        for (i, u) in parsed.units.iter().enumerate() {
            covered.push((u.start_line, u.end_line));
            covered.extend(
                helpers_for(&parsed, i, &[])
                    .iter()
                    .filter(|h| h.path == path)
                    .map(|h| (h.start_line, h.end_line)),
            );
        }
        let mut rest = Vec::new();
        uncovered(root, &covered, src, &mut rest);
        parsed.rest = rest;
    }
    Some(parsed)
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
    let mut seen: HashSet<&str> = unit.hooks.iter().map(|h| h.name.as_str()).collect();
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

fn lines(node: Node) -> (usize, usize) {
    (node.start_position().row + 1, node.end_position().row + 1)
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
    let (start_line, end_line) = lines(node);
    Decl {
        name: name.to_string(),
        path: path.to_string(),
        start_line,
        end_line,
        code: text(node, src).to_string(),
        idents: idents(node, src),
    }
}

const STATEMENT_PARENTS: [&str; 5] = [
    "program",
    "source_file",
    "statement_block",
    "block",
    "statement_list",
];
const TRIVIAL: [&str; 6] = [
    "comment",
    "package_clause",
    "import_declaration",
    "import_statement",
    "hash_bang_line",
    "empty_statement",
];

/// Collects statements that share no line with `covered`, descending into
/// statements that are only partly covered, such as a describe block.
fn uncovered(node: Node, covered: &[(usize, usize)], src: &[u8], out: &mut Vec<Snippet>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let (s, e) = lines(child);
        if covered.iter().any(|&(a, b)| a <= s && e <= b) {
            continue;
        }
        if covered.iter().any(|&(a, b)| a <= e && s <= b) {
            uncovered(child, covered, src, out);
        } else if STATEMENT_PARENTS.contains(&node.kind()) && !TRIVIAL.contains(&child.kind()) {
            out.push(Snippet {
                start_line: s,
                end_line: e,
                code: text(child, src).to_string(),
            });
        }
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
        lang_decls(lang, child, src, path, &mut out);
    }
    out
}

fn lang_decls(lang: Lang, node: Node, src: &[u8], path: &str, out: &mut Vec<Decl>) {
    match lang {
        Lang::Go => go_decls(node, src, path, out),
        _ => js_decls(node, src, path, out),
    }
}

fn go_decls(node: Node, src: &[u8], path: &str, out: &mut Vec<Decl>) {
    let mut names = BTreeSet::new();
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name").map(|n| text(n, src))
                && !is_go_test_name(name)
                && !name.starts_with("Benchmark")
                && !name.starts_with("Fuzz")
            {
                names.insert(name.to_string());
            }
        }
        "type_declaration" | "var_declaration" | "const_declaration" => {
            let mut cursor = node.walk();
            for spec in node.named_children(&mut cursor) {
                let mut c = spec.walk();
                for n in spec.children_by_field_name("name", &mut c) {
                    names.insert(text(n, src).to_string());
                }
            }
        }
        "short_var_declaration" => {
            if let Some(left) = node.child_by_field_name("left") {
                let mut c = left.walk();
                for n in left.named_children(&mut c) {
                    names.insert(text(n, src).to_string());
                }
            }
        }
        _ => {}
    }
    // `var _ = Describe(...)` holds Ginkgo specs, not a helper anyone calls.
    names.remove("_");
    for name in names {
        out.push(decl(&name, node, src, path));
    }
}

/// testify's per-suite setup methods, run around each test method.
const TESTIFY_HOOKS: [&str; 8] = [
    "SetupSuite",
    "SetupTest",
    "SetupSubTest",
    "BeforeTest",
    "AfterTest",
    "TearDownSubTest",
    "TearDownTest",
    "TearDownSuite",
];

fn receiver_type<'a>(method: Node, src: &'a [u8]) -> Option<&'a str> {
    let mut stack = vec![method.child_by_field_name("receiver")?];
    let mut cursor = method.walk();
    while let Some(n) = stack.pop() {
        if n.kind() == "type_identifier" {
            return Some(text(n, src));
        }
        stack.extend(n.named_children(&mut cursor));
    }
    None
}

/// `func TestXxx(t *testing.T)`, and testify suite methods `func (s *S) TestXxx()`
/// carrying their suite's setup methods as hooks.
fn go_tests(root: Node, src: &[u8], path: &str) -> Vec<Unit> {
    let mut cursor = root.walk();
    let top: Vec<Node> = root.named_children(&mut cursor).collect();
    let name_of = |n: Node| n.child_by_field_name("name").map(|n| text(n, src));
    let mut suite_hooks: HashMap<&str, Vec<Decl>> = HashMap::new();
    for &n in &top {
        if n.kind() == "method_declaration"
            && let Some(name) = name_of(n).filter(|m| TESTIFY_HOOKS.contains(m))
            && let Some(recv) = receiver_type(n, src)
        {
            suite_hooks
                .entry(recv)
                .or_default()
                .push(decl(name, n, src, path));
        }
    }
    let mut out = Vec::new();
    for &n in &top {
        let Some(name) = name_of(n).filter(|m| is_go_test_name(m)) else {
            continue;
        };
        let (name, hooks) = match n.kind() {
            "function_declaration" => (name.to_string(), Vec::new()),
            "method_declaration" => {
                let Some(recv) = receiver_type(n, src) else {
                    continue;
                };
                let hooks = suite_hooks.get(recv).cloned().unwrap_or_default();
                (format!("{recv}.{name}"), hooks)
            }
            _ => continue,
        };
        let (start_line, end_line) = lines(n);
        out.push(Unit {
            name,
            start_line,
            end_line,
            code: text(n, src).to_string(),
            idents: idents(n, src),
            hooks,
        });
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Unit,
    Container,
    Hook,
}

/// The names a spec-style test library (`describe`/`it`) uses.
struct Dialect {
    units: &'static [&'static str],
    containers: &'static [&'static str],
    hooks: &'static [&'static str],
}

impl Dialect {
    fn role(&self, name: &str) -> Option<Role> {
        let find = |n: &str| {
            if self.units.contains(&n) {
                Some(Role::Unit)
            } else if self.containers.contains(&n) {
                Some(Role::Container)
            } else if self.hooks.contains(&n) {
                Some(Role::Hook)
            } else {
                None
            }
        };
        // Focused and pending forms: fit, xdescribe, FIt, PDescribe.
        find(name).or_else(|| find(name.strip_prefix(['f', 'x', 'F', 'P', 'X'])?))
    }
}

/// Jest, Vitest, Jasmine, mocha (BDD and TDD), node:test, ava and Playwright.
const JS: Dialect = Dialect {
    units: &["it", "test", "specify"],
    containers: &["describe", "context", "suite"],
    hooks: &[
        "beforeEach",
        "beforeAll",
        "afterEach",
        "afterAll",
        "before",
        "after",
        "setup",
        "teardown",
        "suiteSetup",
        "suiteTeardown",
    ],
};

/// Ginkgo. A `DescribeTable` is judged whole: its entries share one body.
const GINKGO: Dialect = Dialect {
    units: &["It", "Specify", "DescribeTable"],
    containers: &["Describe", "Context", "When"],
    hooks: &[
        "BeforeEach",
        "AfterEach",
        "JustBeforeEach",
        "JustAfterEach",
        "BeforeAll",
        "AfterAll",
    ],
};

/// The role and name of a call such as `it(...)`, `describe.only(...)`,
/// `it.each(t)(...)`, Playwright's `test.beforeEach(...)` or Go's `ginkgo.It(...)`.
fn call_role<'a>(lang: Lang, d: &Dialect, call: Node, src: &'a [u8]) -> Option<(Role, &'a str)> {
    let mut f = call.child_by_field_name("function")?;
    if lang == Lang::Go {
        if f.kind() == "selector_expression" {
            f = f.child_by_field_name("field")?;
        }
        let name = text(f, src);
        return (f.kind() == "identifier" || f.kind() == "field_identifier")
            .then(|| d.role(name).map(|r| (r, name)))
            .flatten();
    }
    let mut by_property = None;
    loop {
        match f.kind() {
            "identifier" => break,
            "member_expression" => {
                let prop = text(f.child_by_field_name("property")?, src);
                if prop == "step" {
                    return None;
                }
                if by_property.is_none() {
                    by_property = d.role(prop).filter(|r| *r != Role::Unit).map(|r| (r, prop));
                }
                f = f.child_by_field_name("object")?;
            }
            "call_expression" => f = f.child_by_field_name("function")?,
            _ => return None,
        }
    }
    let name = text(f, src);
    let role = d.role(name)?;
    Some(by_property.unwrap_or((role, name)))
}

fn call_title(call: Node, src: &[u8]) -> String {
    let arg = call
        .child_by_field_name("arguments")
        .and_then(|a| a.named_child(0))
        .map(|a| text(a, src).to_string())
        .unwrap_or_default();
    arg.trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_string()
}

/// Spec-style tests under `roots`, named by their container path, each with
/// the hooks and declarations of the blocks around it.
fn spec_tests<'t>(
    lang: Lang,
    d: &Dialect,
    roots: impl Iterator<Item = Node<'t>>,
    src: &[u8],
    path: &str,
) -> (Vec<Unit>, Vec<Vec<Decl>>) {
    let mut units = Vec::new();
    let mut scoped = Vec::new();
    let mut stack: Vec<Node> = roots.collect();
    stack.reverse();
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        if node.kind() == "call_expression"
            && matches!(call_role(lang, d, node, src), Some((Role::Unit, _)))
        {
            let mut titles = vec![call_title(node, src)];
            let mut hooks = Vec::new();
            let mut decls = Vec::new();
            let mut up = node.parent();
            while let Some(a) = up {
                if a.kind() == "call_expression"
                    && matches!(call_role(lang, d, a, src), Some((Role::Container, _)))
                {
                    titles.push(call_title(a, src));
                }
                if STATEMENT_PARENTS.contains(&a.kind()) {
                    let mut c = a.walk();
                    for stmt in a.named_children(&mut c) {
                        lang_decls(lang, stmt, src, path, &mut decls);
                        let call = stmt.named_child(0).filter(|n| {
                            stmt.kind() == "expression_statement" && n.kind() == "call_expression"
                        });
                        if let Some(call) = call
                            && let Some((Role::Hook, name)) = call_role(lang, d, call, src)
                        {
                            hooks.push(decl(name, call, src, path));
                        }
                    }
                }
                up = a.parent();
            }
            hooks.reverse();
            titles.reverse();
            let (start_line, end_line) = lines(node);
            units.push(Unit {
                name: titles.join(" > "),
                start_line,
                end_line,
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

    fn unit_names(p: &Parsed) -> Vec<(&str, usize)> {
        p.units
            .iter()
            .map(|u| (u.name.as_str(), u.start_line))
            .collect()
    }

    #[test]
    fn testify_suite_methods_are_tests_with_their_setup_as_hooks() {
        let src = "package q\n\ntype QueueSuite struct{ suite.Suite; q *Queue }\n\nfunc TestQueue(t *testing.T) { suite.Run(t, new(QueueSuite)) }\n\nfunc (s *QueueSuite) SetupTest() { s.q = New() }\n\nfunc (s *QueueSuite) TestDrains() {\n\ts.drain()\n}\n\nfunc (s *QueueSuite) drain() {}\n\nfunc (o *Other) SetupTest() {}\n";
        let p = parse(Lang::Go, "q_test.go", src).unwrap();
        assert_eq!(
            unit_names(&p),
            [("TestQueue", 5), ("QueueSuite.TestDrains", 9)]
        );
        assert_eq!(
            names(&helpers_for(&p, 1, &[])),
            ["SetupTest", "QueueSuite", "drain"]
        );
    }

    #[test]
    fn ginkgo_specs_are_tests_named_by_their_containers() {
        let src = "package q\n\nfunc TestQ(t *testing.T) { RunSpecs(t, \"q\") }\n\nvar _ = Describe(\"queue\", func() {\n\tvar q *Queue\n\tBeforeEach(func() { q = newQueue() })\n\n\tContext(\"when empty\", func() {\n\t\tFIt(\"drains\", func() {\n\t\t\tExpect(q.Len()).To(Equal(0))\n\t\t})\n\t})\n\n\tDescribeTable(\"sizes\", func(n int) {}, Entry(\"one\", 1))\n})\n\nfunc newQueue() *Queue { return nil }\n";
        let p = parse(Lang::Go, "q_test.go", src).unwrap();
        assert_eq!(
            unit_names(&p),
            [
                ("TestQ", 3),
                ("queue > when empty > drains", 10),
                ("queue > sizes", 15)
            ]
        );
        assert_eq!(
            names(&helpers_for(&p, 1, &[])),
            ["BeforeEach", "q", "newQueue"]
        );
        assert!(p.decls.iter().all(|d| d.name != "_"));
        assert!(p.rest.is_empty(), "{:?}", p.rest);
    }

    #[test]
    fn mocha_jasmine_and_node_test_names_are_recognized() {
        let src = "suite('q', () => {\n  before(() => { open(); });\n  context('empty', () => {\n    specify('drains', () => {});\n    xit('skipped', () => {});\n  });\n  test('size', (t) => { t.test('sub', () => {}); });\n});\nfunction open() {}\n";
        let p = parse(Lang::JavaScript, "q.test.js", src).unwrap();
        assert_eq!(
            unit_names(&p),
            [
                ("q > empty > drains", 4),
                ("q > empty > skipped", 5),
                ("q > size", 7)
            ]
        );
        assert_eq!(names(&helpers_for(&p, 0, &[])), ["before", "open"]);
    }

    #[test]
    fn code_outside_every_test_and_helper_is_kept_as_rest() {
        let src = "import { x } from 'y';\n\n// a comment\nconst used = 1;\nconst unused = 2;\n\ndescribe('q', () => {\n  it('a', () => { expect(used).toBe(1); });\n  scenario('unknown library', async () => {\n    await sleep(100);\n  });\n});\n";
        let p = parse(Lang::TypeScript, "q.test.ts", src).unwrap();
        let rest: Vec<_> = p
            .rest
            .iter()
            .map(|s| (s.start_line, s.end_line, s.code.as_str()))
            .collect();
        assert_eq!(
            rest,
            [
                (5, 5, "const unused = 2;"),
                (
                    9,
                    11,
                    "scenario('unknown library', async () => {\n    await sleep(100);\n  });"
                )
            ]
        );
        let go = parse(Lang::Go, "q_test.go", GO).unwrap();
        let rest: Vec<_> = go.rest.iter().map(|s| s.start_line).collect();
        assert_eq!(rest, [11, 13, 20], "unused, TestMain and Testlowercase");
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
