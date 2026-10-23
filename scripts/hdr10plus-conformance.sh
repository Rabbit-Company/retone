#!/usr/bin/env bash
# Checks the HDR10+ AV1 conformance streams: valid streams must pass retone check, and every
# invalid stream must fail with the problem it was made to have.
#
#   scripts/hdr10plus-conformance.sh <folder> [retone binary]
#
# The streams are in conformance/conformance_files of https://github.com/AOMediaCodec/av1-hdr10plus
# and stored with Git LFS, so download them from
# https://media.githubusercontent.com/media/AOMediaCodec/av1-hdr10plus/main/conformance/conformance_files/<path>.
set -u
folder=${1:?usage: hdr10plus-conformance.sh <folder> [retone binary]}
retone=${2:-retone}

expected() {
    case "$1" in
        invalid_duplicate_metadata_3.obu) echo placement ;;
        invalid_duplicate_metadata*) echo duplicate ;;
        invalid_missing_metadata*) echo missing ;;
        invalid_obu_placement*) echo placement ;;
        *_invalid_application_identifier.obu) echo application_identifier ;;
        *_invalid_application_mode.obu) echo application_mode ;;
        *_invalid_color_primaries.obu) echo color_primaries ;;
        *_invalid_country_code.obu) echo country_code ;;
        *_invalid_matrix_coefficients.obu) echo matrix_coefficients ;;
        *_invalid_provider_code.obu) echo provider_code ;;
        *_invalid_provider_oriented_code.obu) echo provider_oriented_code ;;
        *_invalid_transfer_characteristics.obu) echo transfer_characteristics ;;
        *_invalid_wrong_metadata_obu_type.obu) echo metadata_type ;;
        *_encrypted.mp4) echo skip ;;
        *) echo valid ;;
    esac
}

passed=0
failed=0
while IFS= read -r input; do
    name=$(basename "$input")
    want=$(expected "$name")
    [ "$want" = skip ] && continue
    output=$("$retone" --progress json check "$input")
    status=$?
    codes=$(echo "$output" | grep '"event":"finding"' | grep '"severity":"error"' | sed 's/.*"code":"\([^"]*\)".*/\1/' | sort -u | tr '\n' ' ')
    if [ "$want" = valid ]; then
        if [ $status -eq 0 ]; then passed=$((passed + 1)); else failed=$((failed + 1)); echo "FAIL $name: valid, but check found $codes"; fi
    elif [ $status -ne 0 ] && [[ " $codes" == *" $want "* ]]; then
        passed=$((passed + 1))
    else
        failed=$((failed + 1))
        echo "FAIL $name: expected $want, check found ${codes:-nothing}"
    fi
done < <(find "$folder" -type f \( -name '*.obu' -o -name '*.mp4' \) | sort)
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ] && [ "$passed" -gt 0 ]
