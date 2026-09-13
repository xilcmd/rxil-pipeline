#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Regenerate the static page and script templates the Rust mixer fills in.

    $XIL_CODEROOT/venv/bin/python tools/templates/extract.py

`timeline_viz._HTML_TEMPLATE` is ~900 lines of CSS and JavaScript, and
`XILP005_daw_export._make_audacity_script` builds a Python helper script from
an f-string. Retyping either by hand would drift. Instead this renders them
through the reference Python itself with a marker in place of every dynamic
value, so the Rust side does a plain substitution and the static text is
byte-identical by construction.

Rerun it whenever the reference Python (XIL_PY_REF in ci.yml) changes either
function, and commit the regenerated files.
"""

from pathlib import Path

from xil_pipeline import XILP005_daw_export as daw
from xil_pipeline import timeline_viz as tv

OUT = Path(__file__).resolve().parents[2] / "crates" / "xil-cli" / "src" / "mix" / "templates"

MARKERS = {
    "tag": "@@XIL_TAG_ESCAPED@@",
    "duration_fmt": "@@XIL_DURATION@@",
    "span_count": "@@XIL_SPAN_COUNT@@",
    "data_json": "@@XIL_DATA_JSON@@",
    "clips_json": "@@XIL_CLIPS_JSON@@",
    "generated_at": "@@XIL_GENERATED_AT@@",
    "slug_js": "@@XIL_SLUG_JS@@",
    "layer_audio_json": "@@XIL_LAYER_AUDIO_JSON@@",
}


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    html = tv._HTML_TEMPLATE.format(
        **MARKERS,
        modal_css=tv._MODAL_CSS, modal_html=tv._MODAL_HTML, modal_js=tv._MODAL_JS,
        transport_css=tv._TRANSPORT_CSS, transport_html=tv._TRANSPORT_HTML, transport_js=tv._TRANSPORT_JS,
        loader_css=tv._LOADER_CSS, loader_html=tv._LOADER_HTML, loader_js=tv._LOADER_JS,
    )
    for marker in MARKERS.values():
        assert html.count(marker) >= 1, marker
    (OUT / "timeline.html.tmpl").write_bytes(html.encode("utf-8"))

    layers = [("@@XIL_L@@", "@@XIL_F@@")]
    for save, name in ((False, "open_in_audacity.py.tmpl"), (True, "open_in_audacity_aup3.py.tmpl")):
        script = daw._make_audacity_script("@@XIL_TAG@@", layers, save_aup3=save, show="@@XIL_SHOW_LABEL@@")
        script = script.replace(repr(layers), "@@XIL_LAYERS_REPR@@")
        assert "@@XIL_LAYERS_REPR@@" in script
        (OUT / name).write_bytes(script.encode("utf-8"))
    print(f"wrote templates to {OUT}")


if __name__ == "__main__":
    main()
