#!/usr/bin/env bash
set -euo pipefail

resolver="/mnt/c/Temp/carbon-rml-resolver-build/code/roblox_modloader/Release/offline_abi_resolver_tests.exe"
studio="/mnt/c/Temp/RobloxStudio-0.732.0.7321040/RobloxStudioBeta.exe"

if [[ ! -f "$resolver" ]]; then
	echo "missing resolver benchmark binary: $resolver" >&2
	exit 2
fi
if [[ ! -f "$studio" ]]; then
	echo "missing fixed Studio fixture: $studio" >&2
	exit 2
fi

studio_win="$(wslpath -w "$studio")"

start_ns="$(date +%s%N)"
if ! output="$($resolver "$studio_win" 2>&1)"; then
	printf '%s\n' "$output" >&2
	exit 1
fi
end_ns="$(date +%s%N)"

for capability in ReflectionLayout DataModelLayout InstanceLayout SignalLayout JobLayout; do
	if [[ "$output" != *"capability=$capability"* ]]; then
		echo "resolver workload did not complete $capability" >&2
		exit 1
	fi
done

elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
echo "METRIC resolver_ms=$elapsed_ms"
