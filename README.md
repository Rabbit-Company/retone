# retone

retone measures, writes, checks and removes HDR10+ and HDR10 metadata in AV1 video. HDR10+ tells a display how bright every scene is, so a TV that cannot reach the brightness the video was mastered for can tone map each scene on its own instead of the whole film at once. AV1 encoders cannot measure this themselves, and converting or remuxing a video often loses it. retone measures it from the source and puts it into the encode, as the HDR10+ AV1 Metadata Handling Specification places it.

| Command       | What it does                                                                                 |
| ------------- | -------------------------------------------------------------------------------------------- |
| `measure`     | Analyses an HDR10 or HLG video and writes HDR10+ metadata for it                             |
| `convert`     | Writes HDR10+ metadata for a Dolby Vision video, scene by scene as its Dolby Vision shots go |
| `apply`       | Writes HDR10+ metadata into an AV1 video                                                     |
| `export`      | Reads the HDR10+ metadata out of an AV1 video                                                |
| `remove`      | Removes HDR10+ metadata, and on request Dolby Vision and static HDR10 metadata               |
| `static`      | Writes static HDR10 metadata: mastering display, MaxCLL and MaxFALL                          |
| `stats`       | Shows HDR10+ metadata scene by scene                                                         |
| `check`       | Finds what the HDR10+ and AV1 specifications forbid in a video or metadata file              |
| `info`        | Shows the HDR formats and metadata of an AV1 video                                           |
| `completions` | Prints a completion script for bash, zsh, fish, elvish or PowerShell                         |
| `manual`      | Prints the manual page in roff format                                                        |

Metadata files use the JSON layout that hdr10plus_tool reads and writes, so they move freely between retone, hdr10plus_tool and HEVC workflows.

## Typical workflow

```sh
# 1. Measure the HDR10 source.
retone measure source.mkv -o hdr10plus.json

# 2. Encode the source to 10 bit AV1 with any encoder, tagged as BT.2020 PQ.
# 3. Put the metadata into the encode and check the result.
retone apply encode.mkv -m hdr10plus.json -o hdr10plus.mkv --verify

# 4. Give it static HDR10 metadata as well, for displays without HDR10+.
retone static hdr10plus.mkv -o final.mkv --mastering-display p3,1000,0.0001 --cll-from hdr10plus.json
```

A source that already has HDR10+, such as a UHD Blu-ray or an earlier encode, can give its metadata to the new encode directly:

```sh
hdr10plus_tool extract source.hevc -o hdr10plus.json    # HEVC source
retone export source.mkv -o hdr10plus.json              # AV1 source
retone apply encode.mkv -m hdr10plus.json -o final.mkv --verify
```

A Dolby Vision source gives its HDR10+ metadata the shots its colourist set:

```sh
retone convert source.mkv -o hdr10plus.json
```

The metadata needs one entry per frame of the encode, so the encode must have the frames of the source, without cuts.

## Installing

Download an archive for your system from the [releases page](https://github.com/Rabbit-Company/retone/releases). Each one holds the `retone` binary, this README, the changelog and the licence. The binaries need no libraries beyond what the system ships with.

| Archive           | System                                                                               |
| ----------------- | ------------------------------------------------------------------------------------ |
| `linux-x86_64-v3` | Linux on Intel Haswell (2013), AMD Excavator (2015) or newer. Pick this when unsure  |
| `linux-x86_64-v4` | Linux on CPUs with AVX-512, such as AMD Zen 4 or newer and Intel Xeon                |
| `linux-x86_64-v2` | Linux on older x86-64 CPUs that the v3 build does not run on                         |
| `linux-aarch64`   | Linux on 64 bit ARM, such as a Raspberry Pi 4 or 5 with a 64 bit OS, or AWS Graviton |
| `macos-aarch64`   | macOS on Apple silicon (M1 and newer)                                                |
| `macos-x86_64`    | macOS on Intel                                                                       |
| `windows-x86_64`  | Windows on x86-64                                                                    |

On Linux and macOS:

```sh
tar -xzf retone-v0.1.0-linux-x86_64-v3.tar.gz
sudo install retone-v0.1.0-linux-x86_64-v3/retone /usr/local/bin/
```

The macOS binaries are not signed. If macOS refuses to open it, clear the quarantine flag once:

```sh
xattr -d com.apple.quarantine /usr/local/bin/retone
```

On Windows, unpack the zip and put `retone.exe` in a folder on the `PATH`.

`SHA256SUMS` on the releases page lists the checksum of every archive:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

To install the manual and completions:

```sh
retone manual > /usr/local/share/man/man1/retone.1
retone completions bash > /etc/bash_completion.d/retone
```

## Building

```sh
cargo build --release
```

The binary is `target/release/retone`. It has no library dependencies.

## Requirements

`measure` and `convert` need `ffmpeg` and `ffprobe` to decode the video. The other commands read and write IVF, low overhead OBU streams and Annex B without any other program, and use `ffmpeg` only to read or write MKV, WebM and MP4, copying the video without decoding it. `apply --verify` needs an `ffmpeg` with the libdav1d decoder. Pass `--ffmpeg <path>` when `ffmpeg` is not on the `PATH`. `ffprobe` is expected next to it.

When the output is a container and the input was one too, audio, subtitles, attachments, chapters and metadata are copied along. Outputs are written under a temporary name and only take their final name once they are complete.

## measure

```sh
retone measure <INPUT> -o <METADATA>
```

`INPUT` is any video ffmpeg decodes that is PQ (HDR10) or HLG with BT.2020 colours, such as an HEVC or ProRes master or an AV1 encode. retone converts the decoded pictures to light itself, so the measurement does not depend on ffmpeg's colour conversion.

| Option                  | Default     | Meaning                                                                      |
| ----------------------- | ----------- | ---------------------------------------------------------------------------- |
| `--scene-threshold <N>` | 0.3         | How different two frames must be to start a new scene, from 0 to 1           |
| `--per-frame`           | off         | Give every frame its own metadata instead of one set per scene               |
| `--transfer <pq\|hlg>`  | from tags   | Treat the video as PQ or HLG whatever its tags say, for files that lost them |
| `--hlg-peak <CD/M²>`    | 1000        | Peak brightness of the display HLG is shown on                               |
| `-j, --threads <N>`     | all threads | Worker threads                                                               |
| `-y, --overwrite`       | off         | Replace an existing output file                                              |

Every frame is measured, and every scene gets one set of values that covers all of its frames, so a display does not change its tone mapping in the middle of a shot. A new scene starts when the distribution of brightness changes by more than the threshold from one frame to the next.

The values follow SMPTE ST 2094-40, in units of 0.1 cd/m²:

| Value                  | Per frame                                             | Per scene |
| ---------------------- | ----------------------------------------------------- | --------- |
| MaxScl                 | Brightest red, green and blue                         | Highest   |
| AverageRGB             | Average of the brightest component of every pixel     | Average   |
| 1, 25, 50, 75, 90, 95% | Percentiles of the brightest component of every pixel | Average   |
| 99% entry              | 99.98th percentile of the brightest component         | Highest   |
| 5% entry               | 99.99th percentile of luminance                       | Highest   |
| 10% entry              | Percentage of pixels with a luminance up to 100 cd/m² | Average   |

ST 2094-40 reserves the 5% and 10% entries. HDR10+ uses them for the two luminance values above, and players such as mpv take the 99.99th percentile of luminance for their tone mapping. On Netflix's 4K HDR10+ conformance stream, `measure` finds the same two scenes as Netflix's metadata, the same values for the 5% and 10% entries and the other values within 2%, measured from the AV1 encode instead of the master.

`measure` writes profile A metadata, which leaves the tone mapping to the display. Profile B, with a tone mapping curve for one target display, is not measured.

HDR10+ needs PQ. An HLG video is measured as it looks on a display of `--hlg-peak` cd/m², as after converting it to PQ for that display, and `apply` refuses HLG encodes.

## convert

```sh
retone convert <INPUT> -o <METADATA>
```

`INPUT` is a Dolby Vision video in HEVC, raw or in MKV, MP4 and other containers ffmpeg reads, a Dolby Vision AV1 video (profile 10), or an RPU file as `dovi_tool extract-rpu` writes it.

| Option                  | Default     | Meaning                                                                   |
| ----------------------- | ----------- | ------------------------------------------------------------------------- |
| `--video <VIDEO>`       | the input   | The video to measure. Needed for an RPU file and for profile 5            |
| `--scene-threshold <N>` | 0.3         | How scenes are found when the Dolby Vision metadata marks no shots        |
| `--transfer <pq\|hlg>`  | from tags   | Treat the video as PQ or HLG whatever its tags say                        |
| `--hlg-peak <CD/M²>`    | 1000        | Peak brightness of the display HLG is shown on, for profiles 8.4 and 10.4 |
| `-j, --threads <N>`     | all threads | Worker threads                                                            |
| `-y, --overwrite`       | off         | Replace an existing output file                                           |

Dolby Vision metadata has the minimum, average and maximum brightness of every shot, but not how the brightness is distributed, which HDR10+ needs. `convert` therefore measures the video as `measure` does and takes the scenes from the Dolby Vision metadata: every shot the colourist marked becomes an HDR10+ scene. When the metadata marks every frame or none, as some tools write it, the shots show as changes of its level 1 metadata, and when that changes from frame to frame too, scenes are found as `measure` finds them. The summary names the source with `scene_source`.

The video is the base layer: HDR10 for profiles 7, 8.1 and 10.1, HLG for 8.4 and 10.4. Profile 5 has no HDR10 base layer, so its HDR10 version has to come with `--video`. The Dolby Vision metadata must have one RPU per frame of the video, in display order. HEVC frames are coded in another order, and their RPUs are put in display order by the order the frames are shown in, as dovi_tool does.

The mastering display, MaxCLL and MaxFALL that the Dolby Vision metadata states are shown next to what was measured, for `static`.

## apply

```sh
retone apply <INPUT> -m <METADATA> -o <OUTPUT>
```

`INPUT` is an AV1 video. `METADATA` is a JSON file from `measure`, `export` or hdr10plus_tool, or an AV1 video whose HDR10+ metadata to copy.

| Option            | Default | Meaning                                                                |
| ----------------- | ------- | ---------------------------------------------------------------------- |
| `--fix-color`     | off     | Tag the video as BT.2020 PQ when its colour description says otherwise |
| `--verify`        | off     | Read the output back and decode it with dav1d before keeping it        |
| `-y, --overwrite` | off     | Replace an existing output file                                        |
| `--fps <RATE>`    | stream  | Frame rate for streams without timestamps, such as OBU files           |

The metadata must have exactly one entry per shown frame, in display order. Every shown frame gets one HDR10+ metadata OBU right before its frame header, as the specification requires, also when the temporal unit holds hidden frames first. Older dav1d versions lose metadata that comes before a hidden frame instead. HDR10+ metadata already in the video is replaced, everything else stays as it was.

HDR10+ needs a video tagged as BT.2020 primaries, PQ transfer and the BT.2020 non-constant luminance matrix. `apply` refuses other tags, since the metadata would describe a different picture than the display shows. Encoders sometimes leave out the tags of a correct PQ encode, ffmpeg's SVT-AV1 wrapper among them. `--fix-color` rewrites the colour description in the sequence header then, without touching anything else.

With `--verify`, retone reads the written video back, checks it like `check` does and compares every frame's metadata, then decodes it with dav1d through ffmpeg, which parses every HDR10+ OBU on the way.

## export

```sh
retone export <INPUT> -o <METADATA>
```

Writes the HDR10+ metadata of every shown frame as JSON. `-o -` writes to standard output. Every shown frame must have metadata.

## remove

```sh
retone remove <INPUT> -o <OUTPUT>
```

| Option             | Meaning                                                              |
| ------------------ | -------------------------------------------------------------------- |
| `--dolby-vision`   | Also remove Dolby Vision metadata                                    |
| `--static`         | Also remove the mastering display and content light level metadata   |
| `--keep-hdr10plus` | Keep the HDR10+ metadata, to remove only what the other options name |

Removing Dolby Vision from a profile 10.1 or 10.4 video leaves an HDR10 or HLG video.

## static

```sh
retone static <INPUT> -o <OUTPUT> --mastering-display <DISPLAY> --cll-from <METADATA>
```

| Option                          | Meaning                                                                |
| ------------------------------- | ---------------------------------------------------------------------- |
| `--mastering-display <DISPLAY>` | The display the video was mastered on                                  |
| `--max-cll <CD/M²>`             | Brightest pixel of the video, MaxCLL. Needs `--max-fall`               |
| `--max-fall <CD/M²>`            | Highest frame average brightness, MaxFALL. Needs `--max-cll`           |
| `--cll-from <METADATA>`         | Take MaxCLL and MaxFALL from HDR10+ metadata, a JSON file or a video   |
| `--fix-color`                   | Tag the video as BT.2020 PQ when its colour description says otherwise |

The mastering display is written as x265 and SVT-AV1 take it, `G(0.265,0.69)B(0.15,0.06)R(0.68,0.32)WP(0.3127,0.329)L(1000,0.0001)`, also in x265's integer units, `G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)`. `p3,1000,0.0001` and `bt2020,4000,0.005` name a Display P3 or BT.2020 display with its maximum and minimum luminance.

MaxCLL is the brightest component of any pixel and MaxFALL the highest frame average of that, both of which HDR10+ metadata holds for every frame. The metadata goes after every sequence header and replaces what was there.

ffmpeg does not copy the mastering display and content light level into the Matroska colour elements. Players that read them from the bitstream, such as those built on dav1d, see them anyway. For players that only look at the container, set them with `mkvpropedit` as well.

## stats

```sh
retone stats <METADATA>
```

Shows every scene of a JSON file or AV1 video with its first frame, length, start time for videos, brightest component, average, 99.98th percentile and the 99.99th percentile of luminance, and the target display for profile B. `--frames` shows every frame. With `--progress json` every scene or frame is a `scene` or `frame` event with all its values.

## check

```sh
retone check <INPUT>
```

`INPUT` is an AV1 video or a JSON file. Errors are what the specifications forbid, and `check` fails when it finds any:

| Code                                                                                | Problem                                                                                            |
| ----------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `missing`, `duplicate`                                                              | A shown frame without HDR10+ metadata, or with more than one                                       |
| `placement`                                                                         | Metadata before the sequence header, inside a frame, before a hidden frame or after the last frame |
| `color_primaries`, `transfer_characteristics`, `matrix_coefficients`                | The video is not tagged as BT.2020 PQ                                                              |
| `country_code`, `provider_code`, `provider_oriented_code`, `application_identifier` | Metadata that is meant as HDR10+ but carries a wrong code                                          |
| `metadata_type`                                                                     | HDR10+ metadata in a metadata OBU of another type than ITU-T T.35                                  |
| `application_mode`, `num_windows`, `value_range`, `percentages` and others          | Values that HDR10+ does not allow                                                                  |
| `damaged`                                                                           | Metadata that ends early                                                                           |

Warnings mark what the specification recommends against or what displays may handle badly: full range video, a chroma sample position other than top left, no mastering display metadata, a MaxCLL below the HDR10+ peak, metadata brighter than the mastering display, a distribution that is not ordered, and streams with several spatial layers, which the specification does not cover. `--all` lists every finding instead of the first five of each kind. With `--progress json` every finding is a `finding` event.

## info

```sh
retone info <INPUT>
```

Shows the HDR formats of an AV1 video, its size and frame rate, colour description, mastering display and content light level, how many frames carry HDR10+ metadata with its profile, scenes and peaks, whether Dolby Vision metadata and film grain synthesis are present, and how many errors `check` would report.

## Errors and JSON output

With `--progress json`, every command writes one JSON object per line to standard output: a `summary` event at the end, `finding`, `scene` and `frame` events where they apply, and an `error` event with a stable `code` when it fails. Common codes besides those of `check`:

| Code                   | Meaning                                                            |
| ---------------------- | ------------------------------------------------------------------ |
| `frame_count_mismatch` | The metadata and the video have different numbers of frames        |
| `not_hdr10`            | The video is not tagged as BT.2020 PQ, see `--fix-color`           |
| `not_hdr`              | `measure` got a video that is neither PQ nor HLG, see `--transfer` |
| `no_metadata`          | The video has no HDR10+ metadata                                   |
| `no_dolby_vision`      | `convert` found no Dolby Vision metadata                           |
| `missing_dolby_vision` | Some frames have no Dolby Vision metadata                          |
| `invalid_dolby_vision` | Dolby Vision metadata that cannot be read                          |
| `missing_metadata`     | Some shown frames of the video have no HDR10+ metadata             |
| `invalid_metadata`     | The JSON file cannot be read or holds values HDR10+ cannot carry   |
| `check_failed`         | `check` found errors                                               |
| `verify_failed`        | The output of `--verify` did not hold up                           |
| `output_exists`        | The output exists, pass `--overwrite` to replace it                |

## Conformance

CI runs retone over two sets of streams:

- The HDR10+ AV1 conformance streams of the AOM specification. `check` passes the valid ones and finds the problem every invalid one was made to have.
- The 242 libaom test vectors. `apply` writes HDR10+ metadata into each, and the result must pass `check`, carry the metadata through `export` and decode to the same pictures. The two vectors with several spatial layers are skipped.

```sh
scripts/hdr10plus-conformance.sh <folder with the AOM streams> target/release/retone
scripts/conformance.sh <folder with the libaom vectors> target/release/retone
```

retone reads AV1 with the [rabbit-av1](https://crates.io/crates/rabbit-av1) library, which it shares with [regrain](https://github.com/Rabbit-Company/regrain), and Dolby Vision with quietvoid's [dolby_vision](https://crates.io/crates/dolby_vision) and [hevc_parser](https://crates.io/crates/hevc_parser), on which dovi_tool is built.

## Licence

MIT
