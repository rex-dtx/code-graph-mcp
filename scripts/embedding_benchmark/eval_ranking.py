#!/usr/bin/env python3
"""End-to-end retrieval benchmark for semantic_code_search.

Drives the REAL ranking pipeline (FTS + vector + RRF + adjusted-score re-rank) by
spawning `code-graph-mcp serve` over stdio, unlike eval_retrieval.py which is
vector-only. The driver/main lives below the helpers (added in the next task).
"""
import json

DB_NS = 10_000_000


def decode_db_idx(gid: int) -> int:
    """Recover the source-DB index encoded into a global node id."""
    return gid // DB_NS


def encode_global(db_idx: int, local_id: int) -> int:
    """Map a server-local node id back into the global namespace."""
    return db_idx * DB_NS + local_id


def parse_tool_result(rpc_response: dict) -> list[int]:
    """Extract ranked LOCAL node_ids from a tools/call response.

    The tool's JSON is wrapped in result.content[0].text. compact=true yields either
    a JSON array of {node_id, ...} (happy path) or an object {results: [...]} for the
    empty / no-match path. Returns [] on error/empty/malformed."""
    if rpc_response.get("error"):
        return []
    result = rpc_response.get("result")
    if not isinstance(result, dict):
        return []
    content = result.get("content")
    if not content:
        return []
    text = content[0].get("text", "")
    try:
        payload = json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return []
    if isinstance(payload, list):
        return [int(it["node_id"]) for it in payload if isinstance(it, dict) and "node_id" in it]
    if isinstance(payload, dict):
        return [int(it["node_id"]) for it in payload.get("results", [])
                if isinstance(it, dict) and "node_id" in it]
    return []
