#!/usr/bin/env bash
set -euo pipefail

resolver="/mnt/c/Temp/carbon-rml-resolver-build/code/roblox_modloader/Release/offline_abi_resolver_tests.exe"
studio="/mnt/c/Temp/RobloxStudio-0.732.0.7321040/RobloxStudioBeta.exe"
project="/mnt/c/Temp/carbon-rml-resolver-build/code/roblox_modloader/offline_abi_resolver_tests.vcxproj"
msbuild="/mnt/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/MSBuild/Current/Bin/MSBuild.exe"

if [[ ! -f "$resolver" ]]; then
	echo "missing resolver benchmark binary: $resolver" >&2
	exit 2
fi
if [[ ! -f "$studio" ]]; then
	echo "missing fixed Studio fixture: $studio" >&2
	exit 2
fi
if [[ ! -f "$project" || ! -f "$msbuild" ]]; then
	echo "missing persistent resolver build inputs" >&2
	exit 2
fi

"$msbuild" "$(wslpath -w "$project")" /nologo /m /p:Configuration=Release /p:Platform=x64 >/dev/null

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
