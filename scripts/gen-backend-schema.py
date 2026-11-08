#!/usr/bin/env python3
# Prints docs/backend-schema.md, derived from the postgres up-migrations.
#
# The schema is what remains after every up-migration has been applied in
# filename order. Nothing connects to a database: each statement is applied to
# an in-memory model of the catalog. A schema-changing statement this model does
# not understand stops the run with an error rather than being skipped, because
# a skipped ALTER would produce a confidently wrong document.
#
# Usage: gen-backend-schema.py [MIGRATIONS_DIR] > docs/backend-schema.md
import os
import re
import sys

DEFAULT_DIR = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "data-service", "migrations", "postgres"
)


class Unsupported(Exception):
    pass


# ---------------------------------------------------------------- lexing

def _close_quote(text, i):
    """Index just past the quoted string or identifier that opens at text[i]."""
    q = text[i]
    j = i + 1
    while True:
        k = text.index(q, j)  # ValueError on an unterminated literal: fail loudly
        if text.startswith(q, k + 1):
            j = k + 2
            continue
        return k + 1


DOLLAR = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)?\$")


def statements(text):
    """Split SQL into statements. Comments are dropped; literals are kept."""
    out, buf, i, n = [], [], 0, len(text)
    while i < n:
        if text.startswith("--", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            j = text.find("*/", i)
            i = n if j < 0 else j + 2
            buf.append(" ")
        elif text[i] in "'\"":
            j = _close_quote(text, i)
            buf.append(text[i:j])
            i = j
        elif text[i] == "$" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")) \
                and DOLLAR.match(text, i):
            tag = DOLLAR.match(text, i).group(0)
            end = text.find(tag, i + len(tag))
            if end < 0:
                raise Unsupported("unterminated dollar-quoted body")
            end += len(tag)
            buf.append(text[i:end])
            i = end
        elif text[i] == ";":
            out.append("".join(buf))
            buf = []
            i += 1
        else:
            buf.append(text[i])
            i += 1
    out.append("".join(buf))
    return [" ".join(s.split()) for s in out if s.strip()]


def split_top(s, sep=","):
    """Split on sep outside parentheses, brackets and literals."""
    parts, cur, depth, i = [], [], 0, 0
    while i < len(s):
        c = s[i]
        if c in "'\"":
            j = _close_quote(s, i)
            cur.append(s[i:j])
            i = j
            continue
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        if c == sep and depth == 0:
            parts.append("".join(cur).strip())
            cur = []
        else:
            cur.append(c)
        i += 1
    parts.append("".join(cur).strip())
    return [p for p in parts if p]


def close_of(s, open_idx):
    """Index of the bracket matching s[open_idx]."""
    depth, i = 0, open_idx
    while i < len(s):
        c = s[i]
        if c in "'\"":
            i = _close_quote(s, i)
            continue
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise Unsupported("unbalanced parentheses: " + s[:80])


def ident(x):
    """PostgreSQL identifier rule: unquoted names fold to lower case."""
    x = x.strip()
    if x.startswith('"') and x.endswith('"'):
        return x[1:-1]
    return x.lower()


def cols_of(s):
    return [ident(x) for x in split_top(s)]


def strip_parens(s):
    s = s.strip()
    if s.startswith("(") and close_of(s, 0) == len(s) - 1:
        return s[1:-1].strip()
    return s


def word_in(name, text):
    return re.search(r"(?<![A-Za-z0-9_])%s(?![A-Za-z0-9_])" % re.escape(name), text) is not None


# ---------------------------------------------------------------- model

class Table:
    def __init__(self, name):
        self.name = name
        self.cols = []    # dicts: name, type, notnull, default, generated
        self.cons = {}    # constraint name -> dict(kind, ...)
        self.uidx = {}    # unique index name -> dict(expr, where)

    def col(self, name):
        for c in self.cols:
            if c["name"] == name:
                return c
        raise Unsupported("no column %s.%s" % (self.name, name))


class Schema:
    def __init__(self):
        self.tables = {}
        self.enums = {}
        self.domains = {}

    def table(self, name):
        if name not in self.tables:
            raise Unsupported("no table " + name)
        return self.tables[name]

    def apply(self, stmt):
        for pat in IGNORED:
            if pat.match(stmt):
                if pat is DO_BLOCK:
                    body = stmt[stmt.find("$"):]
                    if re.search(r"\b(ALTER|CREATE|DROP)\b", body, re.I):
                        raise Unsupported("DDL inside DO block")
                return
        m = re.match(r"CREATE\s+TABLE\s+(IF\s+NOT\s+EXISTS\s+)?(\S+?)\s*\((.*)\)$", stmt, re.I)
        if m:
            return self._create_table(m.group(2), m.group(3), bool(m.group(1)))
        m = re.match(r"ALTER\s+TABLE\s+(IF\s+EXISTS\s+)?(ONLY\s+)?(\S+?)\s+(.*)$", stmt, re.I)
        if m:
            return self._alter_table(ident(m.group(3)), m.group(4))
        m = re.match(r"CREATE\s+TYPE\s+(\S+)\s+AS\s+ENUM\s*\((.*)\)$", stmt, re.I)
        if m:
            labels = [x[1:-1] if x.startswith("'") else x for x in split_top(m.group(2))]
            self.enums[ident(m.group(1))] = labels
            return
        m = re.match(r"CREATE\s+DOMAIN\s+(\S+)\s+AS\s+(.+?)(\s+CHECK\s*\(.*\))?$", stmt, re.I)
        if m:
            check = m.group(3).strip() if m.group(3) else ""
            self.domains[ident(m.group(1))] = " ".join(
                (m.group(2).upper() + " " + check).split())
            return
        m = re.match(r"CREATE\s+UNIQUE\s+INDEX\s+(IF\s+NOT\s+EXISTS\s+)?(\S+)\s+ON\s+(ONLY\s+)?(\S+?)\s*(USING\s+\w+\s*)?\(", stmt, re.I)
        if m:
            return self._unique_index(stmt, m)
        m = re.match(r"DROP\s+INDEX\s+(IF\s+EXISTS\s+)?(\S+)", stmt, re.I)
        if m:
            name = ident(m.group(2))
            for t in self.tables.values():
                t.uidx.pop(name, None)
            return
        m = re.match(r"DROP\s+TABLE\s+(IF\s+EXISTS\s+)?(.+?)(\s+CASCADE)?$", stmt, re.I)
        if m:
            for name in split_top(m.group(2)):
                name = ident(name)
                if name not in self.tables:
                    if m.group(1):
                        continue
                    raise Unsupported("drop of missing table " + name)
                del self.tables[name]
                for t in self.tables.values():  # PostgreSQL drops dependent FKs
                    for k in [k for k, c in t.cons.items()
                              if c["kind"] == "FOREIGN KEY" and c["ref"] == name]:
                        del t.cons[k]
            return
        m = re.match(r"DROP\s+(TYPE|DOMAIN)\s+(IF\s+EXISTS\s+)?(\S+)", stmt, re.I)
        if m:
            name = ident(m.group(3))
            self.enums.pop(name, None)
            self.domains.pop(name, None)
            return
        raise Unsupported("statement not modelled: " + stmt[:120])

    # -- statements

    def _create_table(self, raw_name, body, if_not_exists):
        name = ident(raw_name)
        if name in self.tables:
            if if_not_exists:
                return
            raise Unsupported("table already exists " + name)
        t = Table(name)
        self.tables[name] = t
        for item in split_top(body):
            if re.match(r"(CONSTRAINT|PRIMARY\s+KEY|UNIQUE|CHECK|FOREIGN\s+KEY)\b", item, re.I):
                explicit, text = None, item
                m = re.match(r"CONSTRAINT\s+(\S+)\s+(.*)$", item, re.I | re.S)
                if m:
                    explicit, text = ident(m.group(1)), m.group(2)
                self._add_constraint(t, parse_constraint(text), explicit)
            else:
                self._add_column(t, item)

    def _add_column(self, t, item):
        col, cons = parse_column(t.name, item)
        if any(c["name"] == col["name"] for c in t.cols):
            raise Unsupported("duplicate column %s.%s" % (t.name, col["name"]))
        t.cols.append(col)
        for con, explicit, base in cons:
            self._add_constraint(t, con, explicit, base)

    def _add_constraint(self, t, con, explicit=None, base=None):
        if base is None:
            base = default_base(t, con)
        if explicit is None:
            name, k = base, 0
            while name in t.cons:
                k += 1
                name = "%s%d" % (base, k)
        else:
            name = explicit
            if name in t.cons:
                raise Unsupported("duplicate constraint " + name)
        if con["kind"] == "PRIMARY KEY":
            for c in con["cols"]:
                t.col(c)["notnull"] = True
        con["name"] = name
        t.cons[name] = con

    def _alter_table(self, tname, rest):
        t = self.table(tname)
        for action in split_top(rest):
            a = action.strip()
            m = re.match(r"ADD\s+(COLUMN\s+)?(IF\s+NOT\s+EXISTS\s+)?(?!(CONSTRAINT|PRIMARY|UNIQUE|CHECK|FOREIGN)\b)(.*)$", a, re.I | re.S)
            if m:
                if m.group(2) and any(c["name"] == ident(m.group(4).split()[0]) for c in t.cols):
                    continue
                self._add_column(t, m.group(4))
                continue
            m = re.match(r"ADD\s+(CONSTRAINT\s+(\S+)\s+)?((PRIMARY|UNIQUE|CHECK|FOREIGN)\b.*)$", a, re.I | re.S)
            if m:
                explicit = ident(m.group(2)) if m.group(2) else None
                self._add_constraint(t, parse_constraint(m.group(3)), explicit)
                continue
            m = re.match(r"DROP\s+COLUMN\s+(IF\s+EXISTS\s+)?(\S+)(\s+CASCADE)?$", a, re.I)
            if m:
                self._drop_column(t, ident(m.group(2)), bool(m.group(1)))
                continue
            m = re.match(r"DROP\s+CONSTRAINT\s+(IF\s+EXISTS\s+)?(\S+)(\s+CASCADE)?$", a, re.I)
            if m:
                name = ident(m.group(2))
                if name in t.cons:
                    del t.cons[name]
                elif not m.group(1):
                    raise Unsupported("no constraint %s on %s" % (name, tname))
                continue
            m = re.match(r"ALTER\s+COLUMN\s+(\S+)\s+(.*)$", a, re.I | re.S)
            if m:
                self._alter_column(t, ident(m.group(1)), m.group(2).strip())
                continue
            m = re.match(r"RENAME\s+COLUMN\s+(\S+)\s+TO\s+(\S+)$", a, re.I)
            if m:
                self._rename_column(t, ident(m.group(1)), ident(m.group(2)))
                continue
            raise Unsupported("ALTER TABLE action not modelled: " + a[:120])

    def _alter_column(self, t, cname, rest):
        col = t.col(cname)
        m = re.match(r"(SET\s+DATA\s+)?TYPE\s+(.+?)(\s+USING\s+.*)?$", rest, re.I | re.S)
        if m:
            col["type"] = " ".join(m.group(2).split()).upper().replace(", ", ",")
            return
        m = re.match(r"SET\s+DEFAULT\s+(.+)$", rest, re.I | re.S)
        if m:
            col["default"] = m.group(1).strip()
            return
        if re.match(r"DROP\s+DEFAULT$", rest, re.I):
            col["default"] = None
            return
        if re.match(r"SET\s+NOT\s+NULL$", rest, re.I):
            col["notnull"] = True
            return
        if re.match(r"DROP\s+NOT\s+NULL$", rest, re.I):
            col["notnull"] = False
            return
        raise Unsupported("ALTER COLUMN not modelled: " + rest[:120])

    def _drop_column(self, t, cname, if_exists):
        if not any(c["name"] == cname for c in t.cols):
            if if_exists:
                return
            raise Unsupported("no column %s.%s" % (t.name, cname))
        t.cols = [c for c in t.cols if c["name"] != cname]
        for k in [k for k, c in t.cons.items() if self._involves(c, cname)]:
            del t.cons[k]
        for k in [k for k, u in t.uidx.items() if word_in(cname, u["expr"] + " " + (u["where"] or ""))]:
            del t.uidx[k]
        for other in self.tables.values():
            for k in [k for k, c in other.cons.items()
                      if c["kind"] == "FOREIGN KEY" and c["ref"] == t.name and cname in c["ref_cols"]]:
                raise Unsupported("column %s.%s is referenced by %s" % (t.name, cname, k))

    def _involves(self, con, cname):
        if con["kind"] == "CHECK":
            return word_in(cname, con["expr"])
        return cname in con["cols"]

    def _rename_column(self, t, old, new):
        col = t.col(old)
        col["name"] = new
        for c in t.cons.values():
            if c["kind"] == "CHECK" and word_in(old, c["expr"]):
                raise Unsupported("rename of column used in CHECK " + c["name"])
            c["cols"] = [new if x == old else x for x in c.get("cols", [])]
        for u in t.uidx.values():
            if word_in(old, u["expr"] + " " + (u["where"] or "")):
                raise Unsupported("rename of column used in unique index")
        for other in self.tables.values():
            for c in other.cons.values():
                if c["kind"] == "FOREIGN KEY" and c["ref"] == t.name:
                    c["ref_cols"] = [new if x == old else x for x in c["ref_cols"]]

    def _unique_index(self, stmt, m):
        name = ident(m.group(2))
        tname = ident(m.group(4))
        t = self.table(tname)
        open_idx = m.end() - 1
        close = close_of(stmt, open_idx)
        expr = stmt[open_idx + 1:close].strip()
        rest = stmt[close + 1:].strip()
        where = None
        if rest:
            w = re.match(r"WHERE\s+(.+)$", rest, re.I)
            if not w:
                raise Unsupported("unique index tail not modelled: " + rest)
            where = w.group(1)
        t.uidx[name] = {"expr": " ".join(expr.split()), "where": where}


def parse_constraint(text):
    t = text.strip()
    m = re.match(r"PRIMARY\s+KEY\s*\((.*)\)$", t, re.I | re.S)
    if m:
        return {"kind": "PRIMARY KEY", "cols": cols_of(m.group(1))}
    m = re.match(r"UNIQUE\s*(NULLS\s+NOT\s+DISTINCT\s*)?\((.*)\)$", t, re.I | re.S)
    if m:
        return {"kind": "UNIQUE", "cols": cols_of(m.group(2)), "nnd": bool(m.group(1))}
    m = re.match(r"CHECK\s*\((.*)\)$", t, re.I | re.S)
    if m:
        return {"kind": "CHECK", "expr": " ".join(m.group(1).split())}
    m = re.match(r"FOREIGN\s+KEY\s*\((.*?)\)\s*REFERENCES\s+(\S+?)\s*\((.*?)\)\s*(.*)$", t, re.I | re.S)
    if m:
        return {"kind": "FOREIGN KEY", "cols": cols_of(m.group(1)), "ref": ident(m.group(2)),
                "ref_cols": cols_of(m.group(3)), "actions": " ".join(m.group(4).split())}
    raise Unsupported("table constraint not modelled: " + t[:120])


def default_base(t, con):
    k = con["kind"]
    if k == "PRIMARY KEY":
        return "%s_pkey" % t.name
    if k == "UNIQUE":
        return "%s_%s_key" % (t.name, "_".join(con["cols"]))
    if k == "FOREIGN KEY":
        return "%s_%s_fkey" % (t.name, "_".join(con["cols"]))
    refs = [c["name"] for c in t.cols if word_in(c["name"], con["expr"])]
    if len(refs) == 1:
        return "%s_%s_check" % (t.name, refs[0])
    return "%s_check" % t.name


COL_KW = re.compile(
    r"(NOT\s+NULL|NULL|DEFAULT|CHECK|REFERENCES|PRIMARY\s+KEY|UNIQUE|CONSTRAINT|GENERATED\s+ALWAYS)\b",
    re.I)


def _prev_word(s, i):
    m = re.search(r"(\w+)\s*$", s[:i])
    return m.group(1).upper() if m else ""


def split_keywords(s):
    """Head text, then (KEYWORD, content) segments for column-level clauses."""
    marks, i = [], 0
    while i < len(s):
        c = s[i]
        if c in "'\"":
            i = _close_quote(s, i)
            continue
        if c in "([":
            i = close_of(s, i) + 1
            continue
        if (i == 0 or not (s[i - 1].isalnum() or s[i - 1] == "_")):
            m = COL_KW.match(s, i)
            if m:
                kw = " ".join(m.group(1).upper().split())
                if kw == "NULL" and _prev_word(s, i) == "SET":
                    i = m.end()
                    continue
                marks.append((i, m.end(), kw))
                i = m.end()
                continue
        i += 1
    head = s[:marks[0][0]].strip() if marks else s.strip()
    segs = []
    for idx, (_, end, kw) in enumerate(marks):
        stop = marks[idx + 1][0] if idx + 1 < len(marks) else len(s)
        content = s[end:stop].strip()
        if kw == "NULL" and segs and segs[-1][0] == "DEFAULT" and not segs[-1][1]:
            segs[-1] = ("DEFAULT", ("NULL " + content).strip())  # DEFAULT NULL, not a clause
            continue
        segs.append((kw, content))
    return head, segs


def parse_column(tname, item):
    """Returns (column dict, [(constraint, explicit name, default base)])."""
    head, segs = split_keywords(item)
    parts = head.split(None, 1)
    if len(parts) != 2:
        raise Unsupported("column without type: " + item[:120])
    name = ident(parts[0])
    col = {"name": name, "type": " ".join(parts[1].split()).upper().replace(", ", ","),
           "notnull": False, "default": None, "generated": None}
    cons, pending = [], None
    for kw, content in segs:
        if kw == "CONSTRAINT":
            pending = ident(content)
            continue
        explicit, pending = pending, None
        if kw == "NOT NULL":
            col["notnull"] = True
        elif kw == "NULL":
            col["notnull"] = False
        elif kw == "DEFAULT":
            col["default"] = content
        elif kw == "GENERATED ALWAYS":
            m = re.match(r"AS\s*\((.*)\)\s*STORED$", content, re.I | re.S)
            if not m:
                raise Unsupported("generated column not modelled: " + content[:120])
            col["generated"] = " ".join(m.group(1).split())
        elif kw == "PRIMARY KEY":
            cons.append(({"kind": "PRIMARY KEY", "cols": [name]}, explicit, None))
        elif kw == "UNIQUE":
            nnd = content.upper().startswith("NULLS NOT DISTINCT")
            cons.append(({"kind": "UNIQUE", "cols": [name], "nnd": nnd}, explicit, None))
        elif kw == "CHECK":
            cons.append(({"kind": "CHECK", "expr": " ".join(strip_parens(content).split())},
                         explicit, "%s_%s_check" % (tname, name)))
        elif kw == "REFERENCES":
            m = re.match(r"(\S+?)\s*\((.*?)\)\s*(.*)$", content, re.S)
            if not m:
                raise Unsupported("REFERENCES not modelled: " + content[:120])
            cons.append(({"kind": "FOREIGN KEY", "cols": [name], "ref": ident(m.group(1)),
                          "ref_cols": cols_of(m.group(2)), "actions": " ".join(m.group(3).split())},
                         explicit, "%s_%s_fkey" % (tname, name)))
        else:
            raise Unsupported("column clause not modelled: " + kw)
    if pending is not None:
        raise Unsupported("dangling CONSTRAINT name in " + item[:120])
    return col, cons


IGNORED = [re.compile(p, re.I) for p in [
    r"(INSERT|UPDATE|DELETE|SELECT|WITH)\b",
    r"COMMENT\s+ON\b",
    r"(GRANT|REVOKE|SET|RESET|BEGIN|COMMIT)\b",
    r"CREATE\s+EXTENSION\b",
    r"CREATE\s+(OR\s+REPLACE\s+)?(FUNCTION|VIEW|TRIGGER|ROLE)\b",
    r"DROP\s+(FUNCTION|VIEW|TRIGGER|ROLE|EXTENSION)\b",
    r"ALTER\s+(ROLE|FUNCTION|DEFAULT)\b",
    r"CREATE\s+(TEMP|TEMPORARY)\s+TABLE\b",
    r"CREATE\s+INDEX\b",
]]
DO_BLOCK = re.compile(r"DO\b", re.I)
IGNORED.append(DO_BLOCK)


# ---------------------------------------------------------------- output

def build(migrations_dir):
    schema = Schema()
    files = sorted(f for f in os.listdir(migrations_dir)
                   if f.endswith(".sql") and not f.endswith(".down.sql"))
    for f in files:
        with open(os.path.join(migrations_dir, f), encoding="utf-8") as fh:
            for stmt in statements(fh.read()):
                try:
                    schema.apply(stmt)
                except Unsupported as e:
                    raise Unsupported("%s: %s" % (f, e))
    return schema


def cell(s):
    return s.replace("|", "\\|")


def render_con(c):
    k = c["kind"]
    if k == "PRIMARY KEY":
        return "PRIMARY KEY (%s)" % ", ".join(c["cols"])
    if k == "UNIQUE":
        return "UNIQUE%s (%s)" % (" NULLS NOT DISTINCT" if c["nnd"] else "", ", ".join(c["cols"]))
    if k == "CHECK":
        return "CHECK (%s)" % c["expr"]
    s = "FOREIGN KEY (%s) REFERENCES %s (%s)" % (", ".join(c["cols"]), c["ref"], ", ".join(c["ref_cols"]))
    return s + (" " + c["actions"] if c["actions"] else "")


def render(schema):
    out = [
        "# Backend schema",
        "",
        "<!-- Generated by scripts/gen-backend-schema.py from data-service/migrations/postgres. Do not edit by hand. -->",
        "",
        "The state after every up-migration has been applied in filename order. Covers enum and domain types, tables, columns, constraints and unique indexes. Non-unique indexes, views, functions, triggers and data are not included.",
        "",
        "## Enum types",
        "",
    ]
    for name in sorted(schema.enums):
        out.append("### `%s`" % name)
        out.append("")
        out.append("Values: " + ", ".join("`'%s'`" % v for v in schema.enums[name]))
        out.append("")
    out += ["## Domain types", ""]
    for name in sorted(schema.domains):
        out.append("- `%s`: `%s`" % (name, schema.domains[name]))
    out += ["", "## Tables", ""]
    for tname in sorted(schema.tables):
        t = schema.tables[tname]
        out += ["### `%s`" % tname, "", "| Column | Type | Nullable | Default |", "|---|---|---|---|"]
        for c in t.cols:
            default = ("GENERATED ALWAYS AS (%s) STORED" % c["generated"]) if c["generated"] else (c["default"] or "")
            out.append("| `%s` | %s | %s | %s |" % (
                c["name"], cell(c["type"]), "no" if c["notnull"] else "yes",
                ("`%s`" % cell(default)) if default else ""))
        out.append("")
        out.append("Constraints:")
        out.append("")
        if not t.cons:
            out.append("- none")
        for name in sorted(t.cons):
            out.append("- `%s`: %s" % (name, render_con(t.cons[name])))
        out.append("")
        out.append("Unique indexes:")
        out.append("")
        if not t.uidx:
            out.append("- none")
        for name in sorted(t.uidx):
            u = t.uidx[name]
            line = "- `%s`: UNIQUE (%s)" % (name, u["expr"])
            if u["where"]:
                line += " WHERE %s" % u["where"]
            out.append(line)
        out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


def main(argv):
    migrations_dir = argv[1] if len(argv) > 1 else DEFAULT_DIR
    try:
        sys.stdout.write(render(build(migrations_dir)))
    except Unsupported as e:
        sys.stderr.write("gen-backend-schema: %s\n" % e)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
