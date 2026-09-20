#!/usr/bin/env python3
"""Generate embeddings.json for the recall benchmark (ADR-016 P0).

Uses the same model and dimensions as production (text-embedding-3-small, 768) through
OpenRouter. The corpus is synthetic, so nothing private leaves the machine. Cost: about
80 short strings, a fraction of a cent. Needs OPENROUTER_API_KEY in the environment.

    OPENROUTER_API_KEY=... python3 gen_embeddings.py
"""
import json, os, sys, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
MODEL, DIMS = "text-embedding-3-small", 768
key = os.environ.get("OPENROUTER_API_KEY")
if not key:
    sys.exit("set OPENROUTER_API_KEY")
corpus = json.load(open(os.path.join(HERE, "corpus.json")))
texts = sorted({m["content"] for m in corpus["memories"]} | {q["query"] for q in corpus["queries"]})
req = urllib.request.Request(
    "https://openrouter.ai/api/v1/embeddings",
    data=json.dumps({"model": MODEL, "input": texts, "dimensions": DIMS}).encode(),
    headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
)
data = json.load(urllib.request.urlopen(req, timeout=120))["data"]
assert len(data) == len(texts)
vectors = {t: [round(x, 6) for x in d["embedding"]] for t, d in zip(texts, sorted(data, key=lambda d: d["index"]))}
assert all(len(v) == DIMS for v in vectors.values())
json.dump({"model": MODEL, "dims": DIMS, "vectors": vectors}, open(os.path.join(HERE, "embeddings.json"), "w"))
print(f"wrote {len(vectors)} vectors")
