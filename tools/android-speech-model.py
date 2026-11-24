"""Stage the pinned multilingual Whisper model for the offline speech verifier."""
import hashlib
import os
from pathlib import Path
import shutil
import sys
import tempfile
from urllib.request import urlopen

NAME = "ggml-small-q5_1.bin"
SHA256 = "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb"
URL = f"https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{NAME}"


def verified(path):
    if not path.is_file():
        return False
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest() == SHA256


def stage(target):
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ym-uncensored"
    model = Path(os.environ.get("YM_ANDROID_SPEECH_MODEL", cache / NAME))
    if not verified(model):
        if "YM_ANDROID_SPEECH_MODEL" in os.environ:
            raise ValueError("YM_ANDROID_SPEECH_MODEL does not match the pinned SHA-256")
        cache.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=cache) as temp:
            pending = Path(temp) / NAME
            with urlopen(URL, timeout=90) as src, pending.open("wb") as dst:
                shutil.copyfileobj(src, dst)
            if not verified(pending):
                raise ValueError("Downloaded Whisper model does not match the pinned SHA-256")
            os.replace(pending, model)
    shutil.copyfile(model, target / NAME)


if __name__ == "__main__":
    stage(Path(sys.argv[1]))
