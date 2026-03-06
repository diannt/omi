#!/usr/bin/env python3
"""
Build a local knowledge-graph SQLite DB from the Omi Flutter app's cached
conversations (shared_preferences.json). Output is consumable by
`persona_server --db <out.sqlite>`.

The Flutter app caches conversation JSONs with `structured.title`,
`structured.overview`, `structured.category`. We run lightweight regex NER
on title+overview, compute co-occurrence within each conversation, and
emit a SQLite DB matching the `local_kg_nodes`/`local_kg_edges` schema
that `graph_analytics::load_local_kg()` expects.

Usage:
  build_kg_from_flutter_cache.py [--prefs <path>] [--out <path>]

Defaults:
  --prefs: Windows: %APPDATA%\me.omi\omi\shared_preferences.json
           macOS/Linux: ~/Library/Application Support/me.omi/omi/shared_preferences.json
  --out:   /tmp/real_kg.sqlite
"""

import argparse
import json
import os
import re
import sqlite3
import sys
from collections import defaultdict

STOPWORDS = {
    'The','This','That','These','Those','Their','They','There','Where',
    'When','What','While','With','Without','Within','Some','Many','Such',
    'Each','Other','Another','Between','Through','During','After','Before',
    'About','Across','Into','From','Upon','Under','Over','Given','Being',
    'Using','Having','Making','Getting','Setting','However','Because',
    'Although','Therefore','Perhaps','Finally','Additionally','Overall',
    'Should','Could','Would','Might','Must','Since','Unless','Until',
}


def default_prefs_path():
    if sys.platform == 'win32':
        return os.path.join(os.environ.get('APPDATA', ''), 'me.omi', 'omi', 'shared_preferences.json')
    # WSL: try Windows side first
    for user in os.listdir('/mnt/c/Users') if os.path.isdir('/mnt/c/Users') else []:
        p = f'/mnt/c/Users/{user}/AppData/Roaming/me.omi/omi/shared_preferences.json'
        if os.path.isfile(p):
            return p
    home = os.path.expanduser('~')
    return os.path.join(home, 'Library', 'Application Support', 'me.omi', 'omi', 'shared_preferences.json')


def extract_entities(text):
    """Regex-based NER: multi-word proper nouns + mid-sentence capitalized words."""
    ents = set()
    # Multi-word: 2+ consecutive capitalized words
    for m in re.finditer(r'\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+)+)\b', text):
        ents.add(m.group(1))
    # Single capitalized, preceded by lowercase+space (i.e. not sentence-initial)
    for m in re.finditer(r'(?<=[a-z,]\s)([A-Z][a-z]{3,})\b', text):
        w = m.group(1)
        if w not in STOPWORDS:
            ents.add(w)
    return ents


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--prefs', default=default_prefs_path(),
                    help='Path to Flutter shared_preferences.json')
    ap.add_argument('--out', default='/tmp/real_kg.sqlite',
                    help='Output SQLite path')
    ap.add_argument('--min-count', type=int, default=2,
                    help='Keep entities appearing in >= N conversations (default 2)')
    ap.add_argument('--min-degree', type=int, default=3,
                    help='Or: keep entities connected to >= N others (default 3)')
    args = ap.parse_args()

    if not os.path.isfile(args.prefs):
        print(f"✗ prefs not found: {args.prefs}", file=sys.stderr)
        sys.exit(1)

    with open(args.prefs) as f:
        prefs = json.load(f)

    uid = prefs.get('flutter.uid', '?')
    convs_raw = prefs.get('flutter.cachedConversations', [])
    convs = [json.loads(c) for c in convs_raw]
    print(f"uid={uid}  ·  {len(convs)} conversations in cache", file=sys.stderr)

    nodes = {}  # nid -> {label, type, count}
    edge_counts = defaultdict(int)

    def get_or_make(label, ntype):
        nid = re.sub(r'[^a-z0-9]+', '_', label.lower()).strip('_')
        if nid not in nodes:
            nodes[nid] = {'label': label, 'type': ntype, 'count': 0}
        nodes[nid]['count'] += 1
        return nid

    for c in convs:
        s = c.get('structured', {}) or {}
        title = s.get('title', '') or ''
        overview = s.get('overview', '') or ''
        category = s.get('category', '') or ''

        conv_ids = set()
        if category:
            conv_ids.add(get_or_make(category, 'category'))
        for e in extract_entities(f"{title}. {overview}"):
            ntype = 'concept' if ' ' in e else 'entity'
            conv_ids.add(get_or_make(e, ntype))

        ids = sorted(conv_ids)
        for i in range(len(ids)):
            for j in range(i+1, len(ids)):
                edge_counts[(ids[i], ids[j])] += 1

    # Prune
    deg = defaultdict(int)
    for (a, b) in edge_counts:
        deg[a] += 1
        deg[b] += 1
    keep = {nid for nid, n in nodes.items()
            if n['count'] >= args.min_count or deg[nid] >= args.min_degree}
    keep |= {nid for nid, n in nodes.items() if n['type'] == 'category'}

    print(f"  {len(nodes)} raw → {len(keep)} after pruning "
          f"(min-count={args.min_count} OR min-degree={args.min_degree})", file=sys.stderr)

    # Write SQLite
    if os.path.exists(args.out):
        os.remove(args.out)
    con = sqlite3.connect(args.out)
    cur = con.cursor()
    cur.execute('''CREATE TABLE local_kg_nodes (
      id INTEGER PRIMARY KEY, nodeId TEXT UNIQUE NOT NULL, label TEXT NOT NULL,
      nodeType TEXT NOT NULL, aliasesJson TEXT, sourceFileIds TEXT,
      createdAt TEXT, updatedAt TEXT)''')
    cur.execute('''CREATE TABLE local_kg_edges (
      id INTEGER PRIMARY KEY, edgeId TEXT UNIQUE NOT NULL,
      sourceNodeId TEXT NOT NULL, targetNodeId TEXT NOT NULL,
      label TEXT NOT NULL, createdAt TEXT)''')

    for nid in keep:
        n = nodes[nid]
        cur.execute(
            "INSERT INTO local_kg_nodes (nodeId, label, nodeType, aliasesJson, sourceFileIds, createdAt, updatedAt) "
            "VALUES (?,?,?,?,?,?,?)",
            (nid, n['label'], n['type'], '[]', '[]', '1970-01-01', '1970-01-01'))

    eix = 0
    kept_edges = 0
    for (a, b), w in edge_counts.items():
        if a in keep and b in keep:
            kept_edges += 1
            # One row per co-occurrence → load_local_kg COUNT(*) yields weight
            for _ in range(w):
                cur.execute(
                    "INSERT INTO local_kg_edges (edgeId, sourceNodeId, targetNodeId, label, createdAt) "
                    "VALUES (?,?,?,?,?)",
                    (f"e{eix}", a, b, 'co-occurs', '1970-01-01'))
                eix += 1

    con.commit()
    con.close()
    print(f"→ {args.out}  ({len(keep)} nodes, {kept_edges} distinct edges, {eix} rows)", file=sys.stderr)
    print(args.out)  # stdout = path for shell pipelines


if __name__ == '__main__':
    main()
