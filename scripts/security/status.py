#!/usr/bin/env python3
"""DBine's security status: the security findings open right now, whatever
found them (OSV-Scanner, Trivy, CodeQL, OpenSSF Scorecard, claude-security), and what the last
scan of each tool covered. A fixed finding leaves the page; there is no
history.

The status lives on the `security-status` branch (STATUS.md, findings.json,
and status.json for the README badge), so CI can update it without
committing to main. See SECURITY.md.

Disclosure rule: an open finding from the source-code review shows only its
severity, kind and component until it's fixed (then it leaves the page).
Dependency CVEs are public already and are shown in full. A secret's value
is never recorded.

Usage (DIR is a checkout of the security-status branch):
  status.py DIR import-claude REPORT_DIR --release vX.Y.Z [--commit SHA]
  status.py DIR import-osv OSV_JSON --commit SHA   # osv-scanner scan source -r --format json
  status.py DIR import-trivy TRIVY_JSON --commit SHA
  status.py DIR import-codeql ALERTS_JSON     # gh api .../code-scanning/alerts?tool_name=CodeQL
  status.py DIR import-scorecard [SCORECARD_JSON]
  status.py DIR coverage --release vX.Y.Z --scanned RANGE --not-covered TEXT
  status.py DIR fix ID                       # fixed: it leaves the page
  status.py DIR dismiss ID --reason TEXT --by NAME
  status.py DIR render
"""
from __future__ import annotations

import argparse
import datetime as dt
import glob
import hashlib
import json
import os
import re
import sys
import urllib.request

SEVERITIES = ["CRITICAL", "HIGH", "MEDIUM", "LOW"]
SCORECARD_REPO = "github.com/addlayer-io/dbine"
REPO_URL = "https://github.com/addlayer-io/dbine"
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))
TRIVY_EXCEPTIONS = os.path.join(ROOT, "trivy-secret.yaml")
OSV_EXCEPTIONS = os.path.join(ROOT, "osv-scanner.toml")

# Scorecard's own risk level per check (https://github.com/ossf/scorecard#checks).
SCORECARD_RISK = {
    "Dangerous-Workflow": "CRITICAL", "Webhooks": "CRITICAL",
    "Binary-Artifacts": "HIGH", "Branch-Protection": "HIGH", "Code-Review": "HIGH",
    "Dependency-Update-Tool": "HIGH", "Maintained": "HIGH", "Signed-Releases": "HIGH",
    "Token-Permissions": "HIGH", "Vulnerabilities": "HIGH",
    "Fuzzing": "MEDIUM", "Packaging": "MEDIUM", "Pinned-Dependencies": "MEDIUM",
    "SAST": "MEDIUM", "Security-Policy": "MEDIUM",
    "CI-Tests": "LOW", "CII-Best-Practices": "LOW", "Contributors": "LOW", "License": "LOW",
    "SBOM": "LOW",
}

SOURCES = {
    "claude-security": "claude-security (source-code review)",
    "osv": "OSV-Scanner (dependency advisories)",
    "trivy": "Trivy (dependencies, secrets, configuration)",
    "codeql": "CodeQL (static analysis)",
}


def today() -> str:
    return dt.date.today().isoformat()


def load(path: str, default):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return default


def save(path: str, data) -> None:
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
        f.write("\n")


class Status:
    def __init__(self, root: str):
        self.root = root
        self.path = os.path.join(root, "findings.json")
        self.data = load(self.path, {"scans": [], "findings": []})

    def save(self) -> None:
        self.data["findings"].sort(key=lambda f: (f["source"], f["id"]))
        save(self.path, self.data)

    def get(self, fid: str) -> dict | None:
        return next((f for f in self.data["findings"] if f["id"] == fid), None)

    def replace(self, source: str, current: list[dict]) -> None:
        """A full scan's findings replace that tool's previous ones; a
        dismissal survives while the tool keeps reporting the finding."""
        old = {f["id"]: f for f in self.data["findings"] if f["source"] == source}
        for f in current:
            prev = old.get(f["id"])
            if prev and prev["status"] == "dismissed":
                f.update({k: prev[k] for k in ("status", "dismissed", "reason", "by")})
            elif prev:
                f["found"] = prev["found"]
        self.data["findings"] = [f for f in self.data["findings"] if f["source"] != source] + current

    def set_scans(self, source: str, scans: list[dict]) -> None:
        self.data["scans"] = [s for s in self.data["scans"] if s["source"] != source] + scans


def component(path: str) -> str:
    parts = path.split("/")
    if parts[:2] == ["crates", "drivers"] and len(parts) > 2:
        return "/".join(parts[:3])
    if parts[0] == "crates" and len(parts) > 1:
        return "/".join(parts[:2])
    return parts[0]


# ---- claude-security -------------------------------------------------------

def import_claude(st: Status, report: str, release: str, commit: str | None) -> None:
    stamp_path = glob.glob(os.path.join(report, "CLAUDE-SECURITY-REVISION-*.json"))
    if not stamp_path:
        sys.exit(f"{report}: no CLAUDE-SECURITY-REVISION-*.json")
    stamp = load(stamp_path[0], {})
    verification = (stamp.get("verification") or {}).get("status")
    if verification != "verified":
        sys.exit(f"{report}: verification is {verification!r}, not 'verified'; scan again")
    rev = commit or stamp["revision"]["commit"]
    md = open(os.path.join(report, "CLAUDE-SECURITY-RESULTS.md"), encoding="utf-8").read()
    rng = re.search(r"\b([0-9a-f]{7,40}|v[\d.]+)\.\.([0-9a-f]{7,40}|v[\d.]+)\b", md[:4000])
    date = stamp["generated_at"][:10]
    rows = [json.loads(l) for l in open(os.path.join(report, "CLAUDE-SECURITY-RESULTS.jsonl"), encoding="utf-8") if l.strip()]
    # The page shows the scans of the latest release only.
    scans = [s for s in st.data["scans"] if s["source"] == "claude-security" and s.get("release") == release]
    scans = [s for s in scans if s.get("scan_id") != stamp.get("scan_id")]
    scans.append({
        "source": "claude-security", "release": release, "scan_id": stamp.get("scan_id"),
        "date": date, "revision": rev[:12],
        "kind": "changes" if stamp.get("mode") == "changes" else "codebase",
        "scope": (rng.group(0) if rng and stamp.get("mode") == "changes"
                  else ", ".join(stamp.get("scope") or []) or "whole repository"),
        "found": len(rows),
    })
    st.set_scans("claude-security", scans)
    if (st.data.get("claude_coverage") or {}).get("release") != release:
        st.data.pop("claude_coverage", None)
    for r in rows:
        fid = "CS-" + r["claudeSecurityPluginFindingId"].split(":")[-1][:10]
        if not st.get(fid):
            st.data["findings"].append({
                "id": fid, "source": "claude-security", "severity": r["severity"].upper(),
                "kind": r.get("category"), "cwe": r.get("cwe_id"),
                "component": component(r["file"]), "found": date, "status": "open",
            })
    print(f"claude-security {release}: {len(rows)} findings from {date} ({rev[:12]}); "
          "run `fix ID` for each one that's fixed")


# ---- Trivy -------------------------------------------------------------------

def import_trivy(st: Status, path: str, commit: str) -> None:
    report = load(path, {})
    date = today()
    cur: list[dict] = []
    for res in report.get("Results") or []:
        target = res.get("Target", "")
        for v in res.get("Vulnerabilities") or []:
            sev = v.get("Severity", "").upper()
            if sev in SEVERITIES:
                cur.append({
                    # Stable per lockfile, so a dismissal sticks to the right one.
                    "id": f"TV-{v['VulnerabilityID']}-{v['PkgName']}-{hashlib.sha256(target.encode()).hexdigest()[:6]}",
                    "source": "trivy", "severity": sev,
                    "kind": "Vulnerable dependency",
                    "title": f"{v['PkgName']} {v.get('InstalledVersion', '')}: {v.get('Title') or v['VulnerabilityID']}",
                    "component": target, "advisory": v["VulnerabilityID"],
                    "fixed_version": v.get("FixedVersion") or None, "found": date, "status": "open",
                })
        for s in res.get("Secrets") or []:
            h = hashlib.sha256(f"{target}:{s.get('RuleID')}:{s.get('StartLine')}".encode()).hexdigest()[:10]
            cur.append({
                "id": f"TS-{h}", "source": "trivy", "severity": s.get("Severity", "HIGH").upper(),
                "kind": f"Committed secret ({s.get('RuleID')})", "title": f"Possible secret in {target}",
                "component": target, "found": date, "status": "open",
            })
        for m in res.get("Misconfigurations") or []:
            if m.get("Status") == "FAIL":
                cur.append({
                    "id": f"TM-{m['ID']}-{hashlib.sha256(target.encode()).hexdigest()[:8]}", "source": "trivy",
                    "severity": m.get("Severity", "LOW").upper(), "kind": "Insecure configuration",
                    "title": f"{m['ID']}: {m.get('Title')}", "component": target, "found": date, "status": "open",
                })
    seen: set[str] = set()
    uniq = [f for f in cur if not (f["id"] in seen or seen.add(f["id"]))]
    st.replace("trivy", uniq)
    st.set_scans("trivy", [{
        "source": "trivy", "date": date, "revision": commit[:12], "kind": "full",
        "scope": "whole repository", "found": len(uniq),
    }])
    print(f"trivy: {len(uniq)} open")


# ---- OSV-Scanner -----------------------------------------------------------------

def cvss_severity(score: str | None) -> str:
    try:
        x = float(score)
    except (TypeError, ValueError):
        return "MEDIUM"  # no score published: not assumed harmless
    return "CRITICAL" if x >= 9 else "HIGH" if x >= 7 else "MEDIUM" if x >= 4 else "LOW"


def import_osv(st: Status, path: str, commit: str) -> None:
    """Dependency advisories from OSV (the database Scorecard uses). One row
    per advisory, package and lockfile; an 'unmaintained' notice isn't a
    vulnerability and is listed apart. Advisories in osv-scanner.toml were
    reviewed: osv-scanner leaves them out and the page lists them with the
    reason."""
    report = load(path, {})
    date = today()
    cur: list[dict] = []
    for res in report.get("results") or []:
        src = os.path.relpath(res["source"]["path"], ROOT) if os.path.isabs(res["source"]["path"]) else res["source"]["path"]
        for p in res.get("packages") or []:
            pkg, ver = p["package"]["name"], p["package"].get("version", "")
            vulns = {v["id"]: v for v in p.get("vulnerabilities") or []}
            for g in p.get("groups") or [{"ids": [i]} for i in vulns]:
                vs = [vulns[i] for i in g["ids"] if i in vulns]
                if not vs:
                    continue
                notice = any((a.get("database_specific") or {}).get("informational") for v in vs for a in v.get("affected") or [])
                fixed = sorted({e["fixed"] for v in vs for a in v.get("affected") or []
                                if (a.get("package") or {}).get("name") == pkg
                                for r in a.get("ranges") or [] for e in r.get("events") or [] if e.get("fixed")})
                summary = next((v.get("summary") for v in vs if v.get("summary")), "") or g["ids"][0]
                cur.append({
                    "id": f"OSV-{sorted(g['ids'])[-1]}-{pkg}-{hashlib.sha256(src.encode()).hexdigest()[:6]}",
                    "source": "osv",
                    "severity": "NOTICE" if notice else cvss_severity(g.get("max_severity")),
                    "kind": "Unmaintained dependency" if notice else "Vulnerable dependency",
                    "title": f"{pkg} {ver}: {summary}", "component": src,
                    "advisory": ", ".join(sorted(g["ids"])),
                    "fixed_version": ", ".join(fixed) or None,
                    "found": date, "status": "notice" if notice else "open",
                })
    seen: set[str] = set()
    cur = [f for f in cur if not (f["id"] in seen or seen.add(f["id"]))]
    st.replace("osv", cur)
    st.set_scans("osv", [{
        "source": "osv", "date": date, "revision": commit[:12], "kind": "full",
        "scope": "every lockfile (Cargo, npm)",
        "found": sum(1 for f in cur if f["status"] == "open"),
    }])
    print(f"osv: {sum(1 for f in cur if f['status'] == 'open')} open, {sum(1 for f in cur if f['status'] == 'notice')} notices")


def osv_exceptions() -> list[dict]:
    """The [[IgnoredVulns]] in osv-scanner.toml (id, reason), read without a
    TOML library so the script runs on any Python 3."""
    try:
        text = open(OSV_EXCEPTIONS, encoding="utf-8").read()
    except FileNotFoundError:
        return []
    out = []
    for block in text.split("[[IgnoredVulns]]")[1:]:
        id_ = re.search(r'^id\s*=\s*"([^"]+)"', block, re.M)
        reason = re.search(r'^reason\s*=\s*"((?:[^"\\]|\\.)*)"', block, re.M)
        if id_:
            out.append({"id": id_.group(1), "why": reason.group(1).replace('\\"', '"') if reason else ""})
    return out


# ---- CodeQL --------------------------------------------------------------------

CODEQL_SEVERITY = {"error": "HIGH", "warning": "MEDIUM", "note": "LOW"}


def import_codeql(st: Status, path: str) -> None:
    """CodeQL's alerts from the code-scanning API (open and dismissed). Like
    the source-code review, an open alert shows only its severity and
    component; a dismissed one shows the rule and the reason given."""
    alerts = load(path, [])
    cur = []
    for a in alerts:
        if a.get("state") not in ("open", "dismissed"):
            continue
        rule = a.get("rule") or {}
        sev = (rule.get("security_severity_level") or "").upper() or CODEQL_SEVERITY.get(rule.get("severity"), "LOW")
        loc = ((a.get("most_recent_instance") or {}).get("location") or {}).get("path", "")
        f = {
            "id": f"CQ-{a['number']}", "source": "codeql", "severity": sev,
            "kind": "Static analysis finding", "component": component(loc) if loc else "repository",
            "found": (a.get("created_at") or today())[:10], "status": "open",
        }
        if a["state"] == "dismissed":
            f.update(status="dismissed", title=f"CodeQL {rule.get('id', '')}: {rule.get('description', '')}",
                     dismissed=(a.get("dismissed_at") or "")[:10],
                     reason=" — ".join(x for x in (a.get("dismissed_reason"), a.get("dismissed_comment")) if x),
                     by=(a.get("dismissed_by") or {}).get("login", ""), component=loc or f["component"])
        cur.append(f)
    # CodeQL keeps its own dismissals; they replace ours wholesale.
    st.data["findings"] = [f for f in st.data["findings"] if f["source"] != "codeql"] + cur
    st.set_scans("codeql", [{
        "source": "codeql", "date": today(), "revision": "", "kind": "full",
        "scope": "Rust, TypeScript/Vue and workflows",
        "found": sum(1 for f in cur if f["status"] == "open"),
    }])
    print(f"codeql: {sum(1 for f in cur if f['status'] == 'open')} open")


# ---- OpenSSF Scorecard ------------------------------------------------------

def import_scorecard(st: Status, path: str | None) -> None:
    if path:
        sc = load(path, {})
    else:
        try:
            with urllib.request.urlopen(f"https://api.scorecard.dev/projects/{SCORECARD_REPO}", timeout=30) as r:
                sc = json.load(r)
        except Exception as e:  # not published yet, or the API is down
            print(f"scorecard: skipped ({e})")
            return
    date = (sc.get("date") or today())[:10]
    rev = ((sc.get("repo") or {}).get("commit") or "")[:12]
    cur = []
    for c in sc.get("checks") or []:
        score = c.get("score")
        if score is None or score < 0 or score >= 10:
            continue
        cur.append({
            "id": f"SC-{c['name']}", "source": "scorecard",
            "severity": SCORECARD_RISK.get(c["name"], "LOW"), "kind": "Repository practice",
            "title": f"{c['name']}: {score}/10 — {c.get('reason', '')}", "component": "repository",
            "advisory": (c.get("documentation") or {}).get("url"), "found": date, "status": "open",
        })
    st.replace("scorecard", cur)
    st.set_scans("scorecard", [{
        "source": "scorecard", "date": date, "revision": rev, "kind": "full",
        "scope": "repository settings and workflows", "score": sc.get("score"),
    }])
    print(f"scorecard: {sc.get('score')} ({len(cur)} checks below 10)")


# ---- Rendering -----------------------------------------------------------------

def trivy_exceptions() -> list[dict]:
    """The allow-rules in trivy-secret.yaml (id, description, path), read
    without a YAML library."""
    try:
        text = open(TRIVY_EXCEPTIONS, encoding="utf-8").read()
    except FileNotFoundError:
        return []
    out = []
    for block in re.split(r"\n  - ", text)[1:]:
        id_ = re.search(r"id:\s*(\S+)", block)
        path = re.search(r"path:\s*(\S+)", block)
        desc = re.search(r"description:\s*>?\s*\n?((?:\s{6}.*\n?)+)", block)
        out.append({
            "id": id_.group(1) if id_ else "",
            "path": path.group(1).replace("\\.", ".") if path else "",
            "why": " ".join(desc.group(1).split()) if desc else "",
        })
    return out


def render(st: Status) -> None:
    fs = st.data["findings"]
    vulns = [f for f in fs if f["source"] != "scorecard"]
    open_v = [f for f in vulns if f["status"] == "open"]
    by_sev = {s: sum(1 for f in open_v if f["severity"] == s) for s in SEVERITIES}

    # Badge: open vulnerabilities from every source, coloured by the worst one.
    parts = [f"{n} {s.lower()}" for s, n in by_sev.items() if n]
    color = ("red" if by_sev["CRITICAL"] or by_sev["HIGH"] else
             "yellow" if by_sev["MEDIUM"] else "yellowgreen" if by_sev["LOW"] else "brightgreen")
    save(os.path.join(st.root, "status.json"), {
        "schemaVersion": 1, "label": "security",
        "message": (" · ".join(parts) + " open") if parts else "0 open", "color": color,
    })

    order = {s: i for i, s in enumerate(SEVERITIES)}
    key = lambda f: (order.get(f["severity"], 9), f["id"])

    def table(rows, cols):
        if not rows:
            return ["_None._"]
        lines = ["| " + " | ".join(c[0] for c in cols) + " |", "|" + "---|" * len(cols)]
        for r in rows:
            lines.append("| " + " | ".join(str(c[1](r)).replace("|", "\\|").replace("\n", " ") for c in cols) + " |")
        return lines

    commit = lambda sha: f"[`{sha[:9]}`]({REPO_URL}/commit/{sha})" if sha else "—"
    out = ["# DBine security status", "",
           "The security findings open in DBine right now, from every tool that looks for them. "
           "A finding leaves this page when it's fixed. "
           f"How the checks work and how to report a vulnerability: [SECURITY.md]({REPO_URL}/blob/main/SECURITY.md).", "",
           f"_Updated {today()}._", "", "## Open findings", "",
           "| Source | Critical | High | Medium | Low |", "|---|---|---|---|---|"]
    for src, name in SOURCES.items():
        o = [f for f in open_v if f["source"] == src]
        out.append(f"| {name} | " + " | ".join(str(sum(1 for f in o if f['severity'] == s)) for s in SEVERITIES) + " |")
    out += ["", "Findings from the source-code review and CodeQL show only their kind and component while they're open.", ""]
    out += table(sorted(open_v, key=key), [
        ("ID", lambda f: f["id"]), ("Severity", lambda f: f["severity"]), ("Source", lambda f: f["source"]),
        ("Finding", lambda f: f.get("title") or f.get("kind") or "Finding"),
        ("Where", lambda f: f"`{f['component']}`"),
        ("Fix", lambda f: f"upgrade to {f['fixed_version']}" if f.get("fixed_version") else "pending"),
    ])

    scans = st.data["scans"]
    out += ["", "## What the last scans covered", ""]
    rows = []
    cs = [s for s in scans if s["source"] == "claude-security"]
    cov = st.data.get("claude_coverage") or {}
    if cs:
        open_cs = sum(1 for f in open_v if f["source"] == "claude-security")
        rows.append({
            "tool": f"claude-security, release {cov.get('release') or cs[-1].get('release', '')}",
            "date": max(s["date"] for s in cs),
            "scope": cov.get("scanned") or "; ".join(sorted({s.get("scope", "") for s in cs})),
            "result": f"{open_cs} open" if open_cs else "0 open",
            "gap": cov.get("not_covered", ""),
        })
    for s in sorted((s for s in scans if s["source"] != "claude-security"), key=lambda s: s["source"]):
        rows.append({
            "tool": s["source"], "date": s["date"],
            "scope": s.get("scope", "") + (f" at {commit(s['revision'])}" if s.get("revision") else ""),
            "result": f"score {s['score']}/10" if "score" in s else f"{s.get('found', 0)} open",
            "gap": "",
        })
    out += table(rows, [("Tool", lambda r: r["tool"]), ("Date", lambda r: r["date"]),
                        ("Covered", lambda r: r["scope"]), ("Result", lambda r: r["result"]),
                        ("Not covered yet", lambda r: r["gap"] or "—")])

    dismissed = [f for f in vulns if f["status"] == "dismissed"]
    exc = trivy_exceptions()
    out += ["", "## Not counted, and why", "",
            "Findings reviewed and found not to affect DBine. Each one says why and who decided.", ""]
    rows = [{"what": f.get("title") or f.get("kind"), "where": f["component"], "why": f.get("reason", ""),
             "by": f.get("by", "")} for f in dismissed]
    rows += [{"what": f"Trivy secret rule exception `{e['id']}`", "where": e["path"], "why": e["why"],
              "by": "trivy-secret.yaml"} for e in exc]
    # Several advisory IDs can share one reason (aliases): one row each reason.
    by_reason: dict[str, list[str]] = {}
    for e in osv_exceptions():
        by_reason.setdefault(e["why"], []).append(e["id"])
    rows += [{"what": "Advisory " + ", ".join(ids), "where": "dependencies", "why": why,
              "by": "osv-scanner.toml"} for why, ids in by_reason.items()]
    out += table(rows, [("Finding", lambda r: r["what"]), ("Where", lambda r: f"`{r['where']}`"),
                        ("Why", lambda r: r["why"]), ("Decided in", lambda r: r["by"])])

    notices = [f for f in fs if f["status"] == "notice"]
    out += ["", "## Maintenance notices", "",
            "Dependencies their authors no longer maintain. Not a vulnerability, so not counted in the badge; "
            "worth replacing over time.", ""]
    out += table(sorted(notices, key=lambda f: f["id"]), [
        ("Dependency", lambda f: f.get("title", f["id"])), ("Where", lambda f: f"`{f['component']}`"),
        ("Advisory", lambda f: f.get("advisory") or "—"),
    ])

    practices = [f for f in fs if f["source"] == "scorecard"]
    sc = next((s for s in scans if s["source"] == "scorecard"), None)
    out += ["", "## Repository practices", "",
            (f"OpenSSF Scorecard: **{sc['score']}/10** ({sc['date']}). " if sc else "")
            + "The checks below 10, with Scorecard's own risk level. They describe how the "
            "repository is run, not a vulnerability in DBine, and aren't counted in the badge.", ""]
    out += table(sorted(practices, key=key), [
        ("Check", lambda f: f.get("title", f["id"])), ("Risk", lambda f: f["severity"]),
        ("Docs", lambda f: f"[link]({f['advisory']})" if f.get("advisory") else "—"),
    ])
    with open(os.path.join(st.root, "STATUS.md"), "w", encoding="utf-8") as fh:
        fh.write("\n".join(out) + "\n")
    print("status: " + (" · ".join(parts) + " open" if parts else "0 open"))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dir")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("import-claude"); p.add_argument("report"); p.add_argument("--release", required=True); p.add_argument("--commit")
    p = sub.add_parser("import-osv"); p.add_argument("json"); p.add_argument("--commit", required=True)
    p = sub.add_parser("import-trivy"); p.add_argument("json"); p.add_argument("--commit", required=True)
    p = sub.add_parser("import-codeql"); p.add_argument("json")
    p = sub.add_parser("import-scorecard"); p.add_argument("json", nargs="?")
    p = sub.add_parser("coverage"); p.add_argument("--release", required=True)
    p.add_argument("--scanned", required=True); p.add_argument("--not-covered", default="")
    p = sub.add_parser("fix"); p.add_argument("id")
    p = sub.add_parser("dismiss"); p.add_argument("id"); p.add_argument("--reason", required=True); p.add_argument("--by", required=True)
    sub.add_parser("render")
    a = ap.parse_args()

    st = Status(a.dir)
    if a.cmd == "import-claude":
        import_claude(st, a.report, a.release, a.commit)
    elif a.cmd == "import-osv":
        import_osv(st, a.json, a.commit)
    elif a.cmd == "import-trivy":
        import_trivy(st, a.json, a.commit)
    elif a.cmd == "import-codeql":
        import_codeql(st, a.json)
    elif a.cmd == "import-scorecard":
        import_scorecard(st, a.json)
    elif a.cmd == "coverage":
        st.data["claude_coverage"] = {"release": a.release, "scanned": a.scanned, "not_covered": a.not_covered}
    elif a.cmd == "fix":
        if not st.get(a.id):
            sys.exit(f"{a.id}: not open")
        st.data["findings"] = [f for f in st.data["findings"] if f["id"] != a.id]
        print(f"{a.id}: fixed, removed")
    elif a.cmd == "dismiss":
        f = st.get(a.id) or sys.exit(f"{a.id}: not open")
        f.update(status="dismissed", dismissed=today(), reason=a.reason, by=a.by)
    st.save()
    render(st)


if __name__ == "__main__":
    main()
