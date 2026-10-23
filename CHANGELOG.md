# Changelog

Error codes, finding codes, JSON field names and the metadata file format stay compatible within a major version. Messages meant for people may change in any release.

## 0.1.0 - 2026-10-23

First release.

### Features

- `measure` analyses PQ and HLG video that ffmpeg decodes and writes HDR10+ profile A metadata with one set of values per scene. On Netflix's 4K HDR10+ conformance stream it finds the same scenes as Netflix's metadata, the same values in the reserved 5% and 10% entries and the other values within 2%.
- `convert` writes HDR10+ metadata for Dolby Vision video in HEVC or AV1, or for an RPU file and its video. The scenes follow the shots of the Dolby Vision metadata, and the values are measured from the base layer.
- `apply` writes HDR10+ metadata into AV1 video, one metadata OBU right before every shown frame as the HDR10+ AV1 Metadata Handling Specification requires. `--fix-color` tags PQ video that lost its colour description as BT.2020 PQ. `--verify` reads the output back and decodes it with dav1d.
- `export` writes the HDR10+ metadata of an AV1 video as JSON, in the layout hdr10plus_tool reads and writes.
- `remove` removes HDR10+ metadata, and with `--dolby-vision` and `--static` also Dolby Vision and static HDR10 metadata.
- `static` writes the mastering display and content light level, with MaxCLL and MaxFALL taken from HDR10+ metadata on request.
- `stats` shows HDR10+ metadata scene by scene or frame by frame.
- `check` finds what the HDR10+ AV1 specification, ST 2094-40 and CTA-861 forbid in a video or metadata file, and what displays may handle badly. It passes the valid HDR10+ AV1 conformance streams and finds the problem of every invalid one.
- `info` shows the HDR formats and metadata of an AV1 video.
- IVF, low overhead OBU streams and Annex B are read and written directly, MKV, WebM and MP4 through ffmpeg.
