# Changelog

Error codes, finding codes, JSON field names and the metadata file format stay compatible within a major version. Messages meant for people may change in any release.

## 0.1.3 - 2026-10-23

### Features

- `apply`, `remove` and `static` report progress, as regrain 0.3.2 does. With `--progress json` they begin with a `start` event, and `progress` events name the `stage` they are in: `read` counts the frames for `apply`, `write` writes the output, and `verify` decodes it with dav1d. `frames`, `total`, `fps` and `eta_seconds` are for the stage, and `percent` is for the whole command, with verifying taking 80% of it. `remove` and `static` write without counting first, so their `total` and `percent` are `null`. On a terminal the progress line shows the stage.

### Other changes

- Uses rabbit-av1 0.1.2, whose decoder check reports the frames it has decoded.

## 0.1.2 - 2026-10-23

### Changes

- `--fix-color` in `apply` and `static` only retags video without a transfer tag, or PQ video with wrong primaries or matrix. Video tagged with another transfer, such as BT.709 SDR, is a different picture that looks washed out as PQ, so it fails with `not_hdr10` unless `--force-fix-color` is given as well. HLG is never retagged.

## 0.1.1 - 2026-10-23

### Features

- `measure` and `convert` report their progress. With `--progress json` they write a `start` event with the expected frame count and picture size, then a `progress` event twice a second with `frames`, `total`, `percent`, `fps`, `elapsed_seconds` and `eta_seconds`, the same events regrain writes. On a terminal the text progress shows the total and the time left. The total comes from the Dolby Vision metadata for `convert` and from the container for `measure`.

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
