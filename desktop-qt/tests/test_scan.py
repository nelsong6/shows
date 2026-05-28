"""Tests for the directory scan (shows.scan)."""

from shows.scan import scan_episodes


def test_scan_finds_videos_natural_sorted(tmp_path):
    (tmp_path / "S01").mkdir()
    for name in ["S01E02.mkv", "S01E10.mkv", "S01E01.mkv", "readme.txt", "poster.jpg"]:
        (tmp_path / "S01" / name).write_text("x")
    (tmp_path / "extra.avi").write_text("x")
    out = scan_episodes(str(tmp_path))
    # video files only; natural order (E02 before E10); backslash relative paths
    assert out == ["extra.avi", "S01\\S01E01.mkv", "S01\\S01E02.mkv", "S01\\S01E10.mkv"]


def test_scan_ignores_non_video_and_empty(tmp_path):
    (tmp_path / "a.txt").write_text("x")
    (tmp_path / "b.nfo").write_text("x")
    assert scan_episodes(str(tmp_path)) == []
