#pragma once

#include "RobloxModLoader/rml_export.hpp"

#define RML_ABI_VERSION 2

namespace rml::version
{
	[[nodiscard]] const char* commit();
	[[nodiscard]] const char* commit_full();
	[[nodiscard]] const char* branch();
	[[nodiscard]] const char* date();
	[[nodiscard]] const char* subject();
	[[nodiscard]] bool dirty();
	[[nodiscard]] const char* string();
}

extern "C" RML_EXPORT const char* __cdecl carbon_rml_build_version();
