#!/usr/bin/env bash
# Writes HDR10+ metadata into every AV1 stream below a folder and checks that the result
# follows the HDR10+ AV1 specification, carries the metadata on every shown frame and decodes
# to the same pictures.
#
#   scripts/conformance.sh <folder> [retone binary]
#
# The vectors are mostly SDR, so they are retagged as PQ with --force-fix-color to test the placement.
#
# The libaom test vectors are listed in libaom's test/test_vectors.cc and can be
# downloaded from https://storage.googleapis.com/aom-test-data/<name>.
set -u
folder=${1:?usage: conformance.sh <folder> [retone binary]}
retone=${2:-retone}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

picture() {
    ffmpeg -v error -nostdin -c:v libdav1d -i "$1" -map 0:V:0 -f md5 - 2>/dev/null
}

# HDR10+ metadata for a number of frames, with a new scene every 10 frames.
metadata() {
    python3 -I - "$1" > "$work/meta.json" <<'EOF'
import json, sys
frames = []
for index in range(int(sys.argv[1])):
    scene = index // 10
    frames.append({
        "LuminanceParameters": {
            "AverageRGB": 100 + scene,
            "LuminanceDistributions": {
                "DistributionIndex": [1, 5, 10, 25, 50, 75, 90, 95, 99],
                "DistributionValues": [1, 9000, 80, 20, 60, 200, 900, 2000, 8000 + scene],
            },
            "MaxScl": [10000 + scene, 9000, 8000],
        },
        "NumberOfWindows": 1,
        "TargetedSystemDisplayMaximumLuminance": 0,
    })
print(json.dumps({"JSONInfo": {"HDR10plusProfile": "A", "Version": "1.0"}, "SceneInfo": frames}))
EOF
}

passed=0
failed=0
skipped=0
while IFS= read -r input; do
    name=${input#"$folder"/}
    extension=${name##*.}
    reference=$(picture "$input")
    if [ -z "$reference" ]; then
        echo "SKIP $name: dav1d cannot decode the original"
        skipped=$((skipped + 1))
        continue
    fi
    info=$("$retone" --progress json info "$input" 2>/dev/null | grep '"event":"summary"')
    frames=$(echo "$info" | sed -n 's/.*"frames":\([0-9]*\).*/\1/p')
    if [ -z "$frames" ]; then
        failed=$((failed + 1))
        echo "FAIL $name: info failed"
        continue
    fi
    metadata "$frames"
    output="$work/out.$extension"
    result=$("$retone" --progress json apply "$input" -m "$work/meta.json" -o "$output" --fix-color --force-fix-color --verify -y)
    if echo "$result" | grep -q '"code":"unsupported_input"'; then
        # Streams with several layers are outside the HDR10+ AV1 specification.
        echo "SKIP $name: $(echo "$result" | sed -n 's/.*"message":"\([^"]*\)".*/\1/p')"
        skipped=$((skipped + 1))
        continue
    fi
    reasons=()
    [ -s "$output" ] || reasons+=("apply failed: $result")
    if [ ${#reasons[@]} -eq 0 ]; then
        "$retone" -q check "$output" || reasons+=("check failed")
        [ "$(picture "$output")" = "$reference" ] || reasons+=("the pictures changed")
        "$retone" -q export "$output" -o "$work/again.json" -y || reasons+=("export failed")
        "$retone" -q remove "$output" -o "$work/removed.$extension" -y || reasons+=("remove failed")
        [ "$(picture "$work/removed.$extension")" = "$reference" ] || reasons+=("remove changed the pictures")
    fi
    if [ ${#reasons[@]} -eq 0 ]; then
        passed=$((passed + 1))
    else
        failed=$((failed + 1))
        echo "FAIL $name: ${reasons[*]}"
    fi
    rm -f "$output" "$work/removed.$extension"
done < <(find "$folder" -type f \( -name '*.ivf' -o -name '*.mkv' -o -name '*.webm' -o -name '*.obu' \) | sort)
echo "$passed passed, $failed failed, $skipped skipped"
[ "$failed" -eq 0 ]
