"""Find an explicit studio track and prepare its audio for the local player."""

import concurrent.futures
import json
from pathlib import Path
import subprocess
import sys
import unicodedata
from urllib.parse import urlsplit

import requests
from ytmusicapi import YTMusic


def key(text):
    text = unicodedata.normalize("NFKC", text).casefold().replace("ё", "е")
    return " ".join("".join(c if c.isalnum() else " " for c in text).split())


def artist_keys(names):
    return {key(name) for name in names if name and key(name)}


def distinct(items):
    return list(dict.fromkeys(items))[:4]


def matches_duration(seconds, duration_ms):
    return isinstance(seconds, (int, float)) and abs(seconds * 1000 - duration_ms) <= 2000


def website(url, host):
    parsed = urlsplit(url)
    return parsed.scheme == "https" and (
        parsed.hostname == host or (parsed.hostname or "").endswith("." + host)
    )


class MusicSession(requests.Session):
    def request(self, method, url, **kwargs):
        kwargs.setdefault("timeout", 3)
        return super().request(method, url, **kwargs)


def youtube_match(query, title, artists, duration):
    tracks = YTMusic(requests_session=MusicSession()).search(
        query, filter="songs", limit=8, ignore_spelling=True
    )
    return distinct(
        "https://www.youtube.com/watch?v=" + track["videoId"]
        for track in sorted(tracks, key=lambda t: t.get("isExplicit") is not True)
        if track.get("isAvailable") is not False
        and track.get("videoId")
        and key(track.get("title", "")) == title
        and artist_keys(a.get("name") for a in track.get("artists", [])) == artists
        and matches_duration(track.get("duration_seconds"), duration)
    )


def yt_dlp(args, timeout):
    return subprocess.run(
        [sys.executable, "-m", "yt_dlp", "--ignore-config", "--socket-timeout", "4",
         "--retries", "0", "--extractor-retries", "0", *args],
        text=True, capture_output=True, check=True, timeout=timeout,
    ).stdout


def soundcloud_match(query, title, artists, duration):
    data = json.loads(yt_dlp(["--flat-playlist", "--dump-single-json", "--", "scsearch6:" + query], 6))
    hits = []
    for track in data.get("entries", []):
        uri = track.get("url") or ""
        if not website(uri, "soundcloud.com") or not matches_duration(track.get("duration"), duration):
            continue
        track_title = key(track.get("title", ""))
        account = key(track.get("uploader") or "")
        if track_title == title and artists == {account}:
            hits.append(uri)
    return distinct(hits)


def bandcamp_match(query, title, artists, duration):
    response = requests.post(
        "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic",
        json={"search_text": query, "search_filter": "t", "full_page": False},
        headers={"Origin": "https://bandcamp.com"}, timeout=3,
    )
    response.raise_for_status()
    urls = [
        row["item_url_path"]
        for row in response.json().get("auto", {}).get("results", [])
        if row.get("type") == "t" and key(row.get("name", "")) == title
        and artist_keys([row.get("band_name")]) == artists
        and row.get("item_url_path") and website(row["item_url_path"], "bandcamp.com")
    ]
    return distinct(urls)  # The downloaded file's duration is checked before use.


def search(provider, query, title, artists, duration):
    try:
        return provider(query, title, artists, duration)
    except (requests.RequestException, subprocess.SubprocessError, ValueError, KeyError):
        return None


def playable_duration(path, timeout):
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "a:0", "-show_entries",
         "stream=duration:format=duration", "-of", "json", str(path)],
        capture_output=True, text=True, check=True, timeout=timeout,
    )
    result = json.loads(probe.stdout)
    streams = result.get("streams", [])
    if not streams:
        raise ValueError("no audio stream")
    return float(streams[0].get("duration") or result.get("format", {}).get("duration", "nan"))


def candidates(title, artists, duration):
    query = " ".join([*artists, title])
    name = key(title)
    names = artist_keys(artists)
    providers = (("Bandcamp", bandcamp_match), ("SoundCloud", soundcloud_match), ("YouTube Music", youtube_match))
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        jobs = [(label, pool.submit(search, find, query, name, names, duration)) for label, find in providers]
        return [{"source": label, "url": url} for label, job in jobs for url in (job.result() or [])]


def download(source, url, duration, folder):
    hosts = {"YouTube Music": "youtube.com", "SoundCloud": "soundcloud.com", "Bandcamp": "bandcamp.com"}
    if source not in hosts or not website(url, hosts[source]):
        return 1
    stage = folder / source.lower().replace(" ", "-")
    stage.mkdir()
    try:
        output = yt_dlp(
            ["--no-playlist", "--no-progress", "--no-mtime", "--max-filesize", "32M",
             "--concurrent-fragments", "4",
             "--format", "bestaudio[ext=m4a]/bestaudio[ext=mp3]",
             "--output", str(stage / "audio.%(ext)s"), "--print", "after_move:filepath",
             "--no-simulate", "--", url],
            8,
        )
        path = Path(output.strip())
        if (path.parent == stage and path.name in ("audio.m4a", "audio.mp3") and path.is_file()
                and matches_duration(playable_duration(path, 2), duration)):
            media = folder / "audio.mp3"
            if path.suffix == ".mp3":
                path.rename(media)
            else:
                subprocess.run(
                    ["ffmpeg", "-nostdin", "-loglevel", "error", "-i", str(path),
                     "-map", "0:a:0", "-map_metadata", "-1", "-c:a", "libmp3lame",
                     "-q:a", "2", "-y", str(media)],
                    capture_output=True, check=True, timeout=6,
                )
            if not matches_duration(playable_duration(media, 2), duration):
                return 1
            print(json.dumps({"file": media.name, "source": source}))
            return 0
    except (OSError, subprocess.SubprocessError, ValueError):
        pass
    return 1


if __name__ == "__main__":
    mode, *args = sys.argv[1:]
    if mode == "--list":
        title, artists, duration = args
        print(json.dumps(candidates(title, json.loads(artists), int(duration))))
    elif mode == "--download":
        source, url, duration, folder = args
        sys.exit(download(source, url, int(duration), Path(folder)))
    else:
        sys.exit(2)
