import json, sys, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "scripts" / "embedding_benchmark"))
from eval_ranking import decode_db_idx, encode_global, parse_tool_result


def _rpc(tool_payload):
    """Wrap a tool's JSON payload the way the MCP server does: result.content[0].text."""
    return {"jsonrpc": "2.0", "id": 7,
            "result": {"content": [{"type": "text", "text": json.dumps(tool_payload)}]}}


def test_decode_encode_roundtrip():
    assert decode_db_idx(10_000_010) == 1
    assert decode_db_idx(10) == 0
    assert encode_global(1, 10) == 10_000_010
    assert encode_global(0, 5) == 5


def test_parse_array_payload_preserves_order():
    payload = [{"node_id": 3, "name": "c"}, {"node_id": 1, "name": "a"}, {"node_id": 2}]
    assert parse_tool_result(_rpc(payload)) == [3, 1, 2]


def test_parse_empty_results_object():
    # compact=true no-match path returns an object, not an array
    payload = {"results": [], "message": "No matching symbols found.", "hint": "..."}
    assert parse_tool_result(_rpc(payload)) == []


def test_parse_error_response_is_empty():
    assert parse_tool_result({"jsonrpc": "2.0", "id": 7,
                              "error": {"code": -32603, "message": "boom"}}) == []


def test_parse_malformed_text_is_empty():
    assert parse_tool_result({"result": {"content": [{"type": "text", "text": "not json"}]}}) == []


def test_parse_missing_content_is_empty():
    assert parse_tool_result({"result": {}}) == []
