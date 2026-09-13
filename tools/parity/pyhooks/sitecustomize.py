# SPDX-License-Identifier: AGPL-3.0-or-later
"""Parity-harness hook: route the Python pipeline's API clients to the mock.

Loaded through PYTHONPATH only by tools/parity/parity.py, and only acts when
XIL_PARITY_MOCK_API names the mock server's base URL. Anthropic honours
ANTHROPIC_BASE_URL on its own; ElevenLabs, sfx-lib's raw httpx calls and gTTS have no such setting, so
their URL builders are patched here, lazily, when the pipeline imports them.
"""

import os
import sys

_MOCK = os.environ.get("XIL_PARITY_MOCK_API")


def _patch(module_name, module):
    if module_name == "elevenlabs.base_client":
        module._get_base_url = lambda *, base_url=None, environment=None: _MOCK
    elif module_name == "xil_pipeline.XILU005_discover_SFX":
        module.ELEVENLABS_BASE = _MOCK
    elif module_name == "gtts.tts":
        # gTTS builds https://translate.google.<tld>/_/TranslateWebserverUi/data/batchexecute
        module._translate_url = lambda tld="com", path="": f"{_MOCK}/{path}"


if _MOCK:
    import importlib.abc
    import importlib.machinery

    class _Finder(importlib.abc.MetaPathFinder):
        _targets = {"elevenlabs.base_client", "gtts.tts", "xil_pipeline.XILU005_discover_SFX"}

        def find_spec(self, fullname, path, target=None):
            if fullname not in self._targets:
                return None
            spec = importlib.machinery.PathFinder.find_spec(fullname, path)
            if spec is None or spec.loader is None:
                return None
            original = spec.loader.exec_module

            def exec_module(module, _orig=original, _name=fullname):
                _orig(module)
                _patch(_name, module)

            spec.loader.exec_module = exec_module
            return spec

    sys.meta_path.insert(0, _Finder())

# Chain to the interpreter's own sitecustomize (Debian ships one), which this
# module shadows by sitting earlier on sys.path.
_here = os.path.dirname(os.path.abspath(__file__))
for _entry in sys.path:
    if os.path.abspath(_entry or ".") == _here:
        continue
    _cand = os.path.join(_entry or ".", "sitecustomize.py")
    if os.path.isfile(_cand):
        with open(_cand, encoding="utf-8") as _f:
            exec(compile(_f.read(), _cand, "exec"), {"__name__": "_chained_sitecustomize", "__file__": _cand})
        break
