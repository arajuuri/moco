#!/usr/bin/env python3
"""Regenerate crates/moco-core/src/catalog_snapshot.json from https://models.dev/api.json.

Keeps a curated set of providers, only models with tool calling, and only the fields moco reads.
Usage: scripts/update-catalog-snapshot.py [path-to-api.json]   (downloads if no path is given)
"""
import json, sys, urllib.request

KEEP = ["anthropic", "openai", "openrouter", "deepseek", "groq", "xai", "mistral", "togetherai", "cerebras", "fireworks-ai"]

def load(path):
    if path:
        return json.load(open(path))
    with urllib.request.urlopen("https://models.dev/api.json", timeout=60) as r:
        return json.load(r)

def trim(data, keep):
    out = {}
    for pid in keep:
        p = data.get(pid)
        if not p:
            continue
        models = {}
        for mid, m in p.get("models", {}).items():
            if not m.get("tool_call"):
                continue
            c = m.get("cost") or {}
            entry = {
                "id": mid,
                "name": m.get("name", mid),
                "reasoning": bool(m.get("reasoning")),
                "limit": {"context": (m.get("limit") or {}).get("context", 0), "output": (m.get("limit") or {}).get("output", 0)},
            }
            if "input" in c and "output" in c:
                entry["cost"] = {k: c[k] for k in ("input", "output", "cache_read", "cache_write") if k in c}
                tiers = [
                    {**{k: t[k] for k in ("input", "output", "cache_read", "cache_write") if k in t}, "tier": {"size": t["tier"]["size"]}}
                    for t in c.get("tiers", [])
                    if t.get("tier", {}).get("type") == "context"
                ]
                if tiers:
                    entry["cost"]["tiers"] = tiers
            models[mid] = entry
        prov = {"id": pid, "name": p.get("name", pid), "env": p.get("env", []), "models": models}
        for k in ("npm", "api"):
            if p.get(k):
                prov[k] = p[k]
        out[pid] = prov
    return out

if __name__ == "__main__":
    snap = trim(load(sys.argv[1] if len(sys.argv) > 1 else None), KEEP)
    dest = "crates/moco-core/src/catalog_snapshot.json"
    with open(dest, "w") as f:
        json.dump(snap, f, separators=(",", ":"), sort_keys=True)
    print(dest, sum(len(p["models"]) for p in snap.values()), "models in", len(snap), "providers")
