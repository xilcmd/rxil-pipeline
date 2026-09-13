# SPDX-License-Identifier: AGPL-3.0-or-later
"""A local stand-in for ElevenLabs, Anthropic and Google Translate TTS.

The parity harness starts one on 127.0.0.1 for each check that sets
`mock_api = true`, points both implementations at it (see pyhooks/ for the
Python side, the XIL_*_BASE_URL variables for Rust) and records every
request as a line of JSON in `<workspace>/_api_requests.jsonl`. That file is
then compared like any other output, so a check proves both sides made the
same calls with the same payloads — not merely that they wrote the same files.

Responses are canned and deterministic. A few inputs trigger failures on
purpose: voice id `bad-voice` answers 400, and any text or prompt containing
`FAIL-500` answers 500 (the pipeline retries server errors with a sleep, so
checks avoid it unless they mean to wait).
"""

from __future__ import annotations

import base64
import json
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

VOICES = {
    "voices": [
        {
            "voice_id": "v-host-001", "name": "Harper Host", "category": "professional",
            "description": "Warm conversational host", "labels": {"gender": "female", "age": "middle_aged",
            "accent": "american", "descriptive": "warm", "use_case": "narration", "language": "en"},
            "sharing": {"name": "Harper (library)", "description": "Library description", "category": "high_quality",
                        "notice_period": 30},
            "verified_languages": [{"language": "en", "model_id": "eleven_v3"}, {"language": "de", "model_id": "eleven_v3"},
                                   {"language": "es", "model_id": "eleven_v3"}, {"language": "fr", "model_id": "eleven_v3"},
                                   {"language": "en", "model_id": "eleven_multilingual_v2"}],
            "high_quality_base_model_ids": ["eleven_v3", "eleven_multilingual_v2"],
            "is_owner": False, "is_bookmarked": True, "permission_on_resource": "admin", "created_at_unix": 1735689600,
        },
        {
            "voice_id": "v-guest-002", "name": "adam", "category": "premade", "description": None,
            "labels": {"gender": "male", "accent": "british"}, "sharing": None, "verified_languages": [],
            "high_quality_base_model_ids": [], "is_owner": True, "is_bookmarked": False,
            "permission_on_resource": None, "created_at_unix": None,
        },
        {
            "voice_id": "v-clone-003", "name": "Zed Clone", "category": "cloned",
            "description": "A cloned voice with a very long description that runs well past sixty characters in length",
            "labels": {}, "is_owner": True, "is_bookmarked": False, "created_at_unix": 1700000000,
        },
    ]
}

# Sound-generation history, two pages. Page two uses the alternate field
# names XILU005 also accepts (generations / id / prompt / settings).
SFX_HISTORY_PAGE1 = {
    "history": [
        {"history_item_id": "h-rain-01", "text": "Heavy rain on a tin roof", "model_id": "eleven_text_to_sound_v2",
         "date_unix": 1780000000, "generation_config": {"duration_seconds": 4.5, "prompt_influence": 0.3},
         "character_count_change_from": 1000, "character_count_change_to": 1180},
        {"history_item_id": "h-door-02", "text": "", "prompt": "Old wooden door creaks open slowly, then a long "
         "unsettling pause before it slams shut in the wind — très dramatique",
         "date_unix": 1790000000, "generation_config": {"duration_seconds": None, "prompt_influence": 1},
         "character_count_change_from": 1180, "character_count_change_to": 2380},
        {"history_item_id": "h-bell-03", "text": "Church bell, distant", "model_id": "",
         "date_unix": 1780000000, "character_count_change_from": 0, "character_count_change_to": 40},
    ],
    "has_more": True,
    "last_history_item_id": "h-bell-03",
}
SFX_HISTORY_PAGE2 = {
    "generations": [
        {"id": "h-wind-04", "prompt": "Wind through pine trees", "created_at_unix": 1785000123,
         "settings": {"duration_seconds": 10, "prompt_influence": 0.75}},
        {"id": "h-null-05", "text": "Static hiss", "date_unix": None},
    ],
    "has_more": False,
}

USER = {
    "user_id": "u-parity", "first_name": "Parity",
    "subscription": {"tier": "creator", "character_count": 12345, "character_limit": 100000,
                     "can_extend_character_limit": True, "allowed_to_extend_character_limit": True,
                     "next_character_count_reset_unix": 1790000000, "voice_limit": 30,
                     "professional_voice_limit": 1, "can_extend_voice_limit": False, "can_use_instant_voice_cloning": True,
                     "can_use_professional_voice_cloning": True, "status": "active", "currency": "usd"},
    "is_new_user": False, "xi_api_key": None, "can_use_delayed_payment_methods": False,
}


def _tone_mp3(freq: int, seconds: float) -> bytes:
    r = subprocess.run(
        ["ffmpeg", "-v", "quiet", "-f", "lavfi", "-i", f"sine=frequency={freq}:duration={seconds}:sample_rate=44100",
         "-ac", "1", "-b:a", "64k", "-f", "mp3", "-"],
        capture_output=True,
    )
    return r.stdout


class _State:
    def __init__(self) -> None:
        self.log_path: Path | None = None
        self.lock = threading.Lock()
        self._audio: dict[tuple[int, float], bytes] = {}

    def audio(self, freq: int, seconds: float) -> bytes:
        key = (freq, seconds)
        if key not in self._audio:
            self._audio[key] = _tone_mp3(freq, seconds)
        return self._audio[key]


STATE = _State()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args) -> None:  # silence the default stderr access log
        return

    def _record(self, body: bytes) -> object:
        parts = urlsplit(self.path)
        ctype = self.headers.get("Content-Type", "")
        parsed: object = None
        if body:
            if "json" in ctype:
                try:
                    parsed = json.loads(body)
                except ValueError:
                    parsed = body.decode("utf-8", "replace")
            elif "multipart/form-data" in ctype:
                from email.parser import BytesParser
                from email.policy import HTTP

                msg = BytesParser(policy=HTTP).parsebytes(
                    b"Content-Type: " + ctype.encode() + b"\r\n\r\n" + body)
                fields = {}
                for part in msg.iter_parts():
                    name = part.get_param("name", header="content-disposition")
                    payload = part.get_payload(decode=True) or b""
                    fn = part.get_filename()
                    fields[name] = {"filename": fn, "size": len(payload)} if fn else payload.decode("utf-8", "replace")
                parsed = fields
            elif "form" in ctype:
                parsed = {k: v for k, v in parse_qs(body.decode()).items()}
            else:
                parsed = body.decode("utf-8", "replace")
        rec = {
            "method": self.command,
            "path": parts.path,
            "query": {k: sorted(v) for k, v in sorted(parse_qs(parts.query).items())},
            "has_api_key": bool(self.headers.get("xi-api-key") or self.headers.get("x-api-key")),
            "body": parsed,
        }
        if STATE.log_path is not None:
            with STATE.lock, open(STATE.log_path, "a", encoding="utf-8") as f:
                f.write(json.dumps(rec, sort_keys=True) + "\n")
        return parsed

    def _send(self, status: int, payload: bytes, ctype: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _json(self, status: int, obj: object) -> None:
        self._send(status, json.dumps(obj).encode(), "application/json")

    def _body(self) -> bytes:
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self) -> None:  # noqa: N802
        self._record(b"")
        path = urlsplit(self.path).path
        if path == "/v1/voices":
            return self._json(200, VOICES)
        if path == "/v1/user":
            return self._json(200, USER)
        if path == "/v1/sound-generation/history":
            key = self.headers.get("xi-api-key") or ""
            if key == "parity-denied-key":
                return self._json(401, {"detail": {"status": "missing_permissions",
                                                   "message": "The API key is missing sound_generation."}})
            if key == "parity-invalid-key":
                return self._json(401, {"detail": {"status": "invalid_api_key", "message": "Invalid API key."}})
            after = parse_qs(urlsplit(self.path).query).get("start_after_history_item_id")
            return self._json(200, SFX_HISTORY_PAGE2 if after == ["h-bell-03"] else SFX_HISTORY_PAGE1)
        return self._json(404, {"detail": {"status": "not_found", "message": path}})

    def do_POST(self) -> None:  # noqa: N802
        body = self._body()
        parsed = self._record(body)
        path = urlsplit(self.path).path
        text = json.dumps(parsed) if not isinstance(parsed, str) else parsed
        if "FAIL-500" in (text or ""):
            return self._json(500, {"detail": {"status": "server_error", "message": "mock failure"}})
        if path.startswith("/v1/text-to-speech/"):
            voice = path.split("/")[3]
            if voice == "bad-voice":
                return self._json(400, {"detail": {"status": "voice_not_found", "message": "A voice with that id was not found."}})
            n = len((parsed or {}).get("text", "")) if isinstance(parsed, dict) else 1
            return self._send(200, STATE.audio(200 + (n % 50) * 10, round(0.3 + (n % 7) * 0.1, 1)), "audio/mpeg")
        if path == "/v1/sound-generation":
            d = float((parsed or {}).get("duration_seconds") or 1.0) if isinstance(parsed, dict) else 1.0
            return self._send(200, STATE.audio(700, round(min(d, 3.0), 1)), "audio/mpeg")
        if path == "/v1/studio/projects":
            return self._json(200, {"project": {
                "project_id": "proj-parity-1", "name": "mock", "create_date_unix": 1790000000,
                "default_title_voice_id": "v", "default_paragraph_voice_id": "v", "default_model_id": "eleven_v3",
                "can_be_downloaded": True, "volume_normalization": False, "state": "default",
                "access_level": "admin"}})
        if path == "/v1/messages":
            prompt = json.dumps(parsed)
            return self._json(200, {
                "id": "msg_parity", "type": "message", "role": "assistant", "model": (parsed or {}).get("model", "m"),
                "content": [{"type": "text", "text": f"Mock post draft ({len(prompt)} prompt bytes).\n\n#podcast #mock"}],
                "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 100, "output_tokens": 20}})
        if path.endswith("/batchexecute"):
            audio = base64.b64encode(STATE.audio(400, 0.5)).decode()
            inner = json.dumps([audio], separators=(",", ":"))
            frame = json.dumps([["wrb.fr", "jQ1olc", inner, None, None, None, "generic"]], separators=(",", ":"))
            payload = f")]}}'\n\n{len(frame)}\n{frame}\n".encode()
            return self._send(200, payload, "application/json; charset=utf-8")
        return self._json(404, {"detail": {"status": "not_found", "message": path}})


class MockApi:
    """Context manager: a threaded mock API on a free local port."""

    def __enter__(self) -> "MockApi":
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return self

    def log_to(self, path: Path | None) -> None:
        STATE.log_path = path

    def __exit__(self, *exc) -> None:
        self.server.shutdown()
        self.server.server_close()
