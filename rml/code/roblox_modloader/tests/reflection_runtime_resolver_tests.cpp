#include "RobloxModLoader/roblox/reflection/runtime_layout_resolver.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/reflection/object.hpp"
#include "RobloxModLoader/roblox/util/name.hpp"
#include <Zydis/Zydis.h>

#include <array>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <span>
#include <iostream>
#include <optional>
#include <vector>

namespace
{
	using rml::roblox::internals::CompatibilityFailure;
	using rml::roblox::internals::resolve_reflection_layout;
	using rml::roblox::internals::ReflectionCapabilities;

	constexpr std::uintptr_t image_base = 0x140000000;
	constexpr std::uintptr_t vft_descriptor = image_base + 0x2000;
	constexpr std::uintptr_t vft_member = image_base + 0x2010;
	constexpr std::uintptr_t vft_property = image_base + 0x2020;
	constexpr std::uintptr_t vft_function = image_base + 0x2030;
	constexpr std::uintptr_t vft_yield_function = image_base + 0x2040;
	constexpr std::uintptr_t vft_event = image_base + 0x2050;
	constexpr std::uintptr_t vft_callback = image_base + 0x2060;
	constexpr std::uintptr_t vft_class_descriptor = image_base + 0x2070;
	constexpr std::uintptr_t get_string_atom_target = image_base + 0x3000;

	constexpr std::uintptr_t vft_target = image_base + 0x2070;

	template <std::size_t N>
	void append_u32(std::array<std::byte, N>& code, std::size_t& size, const std::uint32_t value)
	{
		std::memcpy(code.data() + size, &value, sizeof(value));
		size += sizeof(value);
	}

	template <std::size_t N>
	void append_constructor_code(
		std::array<std::byte, N>& code,
		std::size_t& size,
		const std::array<std::uint32_t, 5>& container_offsets,
		const std::uint32_t base_class_offset,
		const std::uint32_t functionality_offset)
	{
		// 1. LEA RAX, [RIP + disp] -> vft_target
		code[size++] = std::byte{0x48};
		code[size++] = std::byte{0x8d};
		code[size++] = std::byte{0x05};
		const auto inst_address = image_base + size - 3;
		const auto target_disp = static_cast<std::int32_t>(vft_target - (inst_address + 7));
		append_u32(code, size, static_cast<std::uint32_t>(target_disp));

		// 2. MOV [RCX], RAX -> sets this_reg = RCX
		code[size++] = std::byte{0x48};
		code[size++] = std::byte{0x89};
		code[size++] = std::byte{0x01};

		// XOR R9, R9 (zero reg)
		code[size++] = std::byte{0x4d};
		code[size++] = std::byte{0x31};
		code[size++] = std::byte{0xc9};

		// 3. For each container offset, LEA R8, [RCX + offset], MOV [R8+0], R9, MOV [R8+8], R9, MOV [R8+0x10], R9
		for (const auto offset : container_offsets)
		{
			// LEA R8, [RCX + offset]
			code[size++] = std::byte{0x4c};
			code[size++] = std::byte{0x8d};
			code[size++] = std::byte{0x81};
			append_u32(code, size, offset);

			// MOV [R8 + 0], R9
			code[size++] = std::byte{0x4d};
			code[size++] = std::byte{0x89};
			code[size++] = std::byte{0x08};

			// MOV [R8 + 8], R9
			code[size++] = std::byte{0x4d};
			code[size++] = std::byte{0x89};
			code[size++] = std::byte{0x48};
			code[size++] = std::byte{0x08};

			// MOV [R8 + 0x10], R9
			code[size++] = std::byte{0x4d};
			code[size++] = std::byte{0x89};
			code[size++] = std::byte{0x48};
			code[size++] = std::byte{0x10};
		}

		// 4. Base class load & store: MOV R8, [RDX + base_class_offset] -> MOV [RCX + base_class_offset], R8
		code[size++] = std::byte{0x4c};
		code[size++] = std::byte{0x8b};
		code[size++] = std::byte{0x82};
		append_u32(code, size, base_class_offset);

		code[size++] = std::byte{0x4c};
		code[size++] = std::byte{0x89};
		code[size++] = std::byte{0x81};
		append_u32(code, size, base_class_offset);

		// 5. Functionality bitwise update: TEST dword ptr [RCX + func_offset], 0x8
		code[size++] = std::byte{0xf7};
		code[size++] = std::byte{0x81};
		append_u32(code, size, functionality_offset);
		append_u32(code, size, 0x08);

		// 6. RET
		code[size++] = std::byte{0xc3};
	}
constexpr std::array<std::byte, 2336> studio_0731_constructor_fixture = {
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5c}, std::byte{0x24}, std::byte{0x18}, std::byte{0x48}, std::byte{0x89}, std::byte{0x54}, std::byte{0x24}, std::byte{0x10}, std::byte{0x48}, std::byte{0x89}, std::byte{0x4c}, std::byte{0x24}, std::byte{0x08}, std::byte{0x55},
	std::byte{0x56}, std::byte{0x57}, std::byte{0x41}, std::byte{0x54}, std::byte{0x41}, std::byte{0x55}, std::byte{0x41}, std::byte{0x56}, std::byte{0x41}, std::byte{0x57}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xec}, std::byte{0x48}, std::byte{0x83}, std::byte{0xec},
	std::byte{0x70}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xd0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xf2}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0xe9}, std::byte{0x8b}, std::byte{0x45}, std::byte{0x60}, std::byte{0x89}, std::byte{0x44}, std::byte{0x24},
	std::byte{0x20}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x75}, std::byte{0x78}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xc6}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xd2}, std::byte{0xe8}, std::byte{0xa0}, std::byte{0xa1}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x8d}, std::byte{0x05}, std::byte{0x91}, std::byte{0x31}, std::byte{0x7e}, std::byte{0x03}, std::byte{0x49}, std::byte{0x89}, std::byte{0x45}, std::byte{0x00}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x5d}, std::byte{0x28}, std::byte{0x48},
	std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0x90}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29}, std::byte{0x45},
	std::byte{0xc0}, std::byte{0x45}, std::byte{0x33}, std::byte{0xff}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x7b}, std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d},
	std::byte{0x46}, std::byte{0x28}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x40}, std::byte{0x4c}, std::byte{0x89},
	std::byte{0x7b}, std::byte{0x48}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x50}, std::byte{0x44}, std::byte{0x88}, std::byte{0x7b}, std::byte{0x58}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08}, std::byte{0x66},
	std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x53},
	std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb}, std::byte{0x0c},
	std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0xd4}, std::byte{0x11}, std::byte{0xa4}, std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x14}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xd7}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0x88}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0x98}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0x88},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x40}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x7b}, std::byte{0x48}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x50}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x58}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08},
	std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b},
	std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb},
	std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0x23}, std::byte{0x11}, std::byte{0xa4}, std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x14}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xd7}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0xe8}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0xa0}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0xe8},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x40}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x7b}, std::byte{0x48}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x50}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x58}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08},
	std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b},
	std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb},
	std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0x73}, std::byte{0x10}, std::byte{0xa4}, std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x14}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xd7}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0x48}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0xa8}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0x48},
	std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x40}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x7b}, std::byte{0x48}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x50}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x58}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08},
	std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b},
	std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb},
	std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0xc3}, std::byte{0x0f}, std::byte{0xa4}, std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x14}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xd7}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0xa8}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0xb0}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0xa8},
	std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x40}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x7b}, std::byte{0x48}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x50}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x58}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08},
	std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b},
	std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb},
	std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0x13}, std::byte{0x0f}, std::byte{0xa4}, std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x14}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xd7}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x86}, std::byte{0x08}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x25}, std::byte{0x00}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x48}, std::byte{0x0b}, std::byte{0x85}, std::byte{0x80}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x89}, std::byte{0x85},
	std::byte{0x08}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0x10}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0x18}, std::byte{0x02},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x85}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x33}, std::byte{0x46}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe0}, std::byte{0x01},
	std::byte{0x41}, std::byte{0x31}, std::byte{0x85}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x85}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b},
	std::byte{0x4e}, std::byte{0x10}, std::byte{0xd1}, std::byte{0xf9}, std::byte{0x03}, std::byte{0xc9}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x06}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d},
	std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x46}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe0}, std::byte{0xf8}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x83}, std::byte{0xe0}, std::byte{0x08},
	std::byte{0x33}, std::byte{0xc1}, std::byte{0x41}, std::byte{0x89}, std::byte{0x85}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe1}, std::byte{0xf0},
	std::byte{0x33}, std::byte{0xc8}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x10}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b},
	std::byte{0x46}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe0}, std::byte{0xe0}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x83}, std::byte{0xe0}, std::byte{0x20}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x41}, std::byte{0x89}, std::byte{0x85}, std::byte{0x20},
	std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe1}, std::byte{0xc0}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x40}, std::byte{0x33},
	std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x56}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe2}, std::byte{0x80}, std::byte{0x33},
	std::byte{0xd1}, std::byte{0x81}, std::byte{0xe2}, std::byte{0x80}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x33}, std::byte{0xd1}, std::byte{0x41}, std::byte{0x89}, std::byte{0x95}, std::byte{0x20}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x41}, std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0xc1}, std::byte{0xf9}, std::byte{0x08}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x01}, std::byte{0x0f}, std::byte{0xb6}, std::byte{0x45}, std::byte{0x70}, std::byte{0xc1}, std::byte{0xe0},
	std::byte{0x02}, std::byte{0x0b}, std::byte{0xc8}, std::byte{0xc1}, std::byte{0xe1}, std::byte{0x08}, std::byte{0x0f}, std::byte{0xb6}, std::byte{0x45}, std::byte{0x68}, std::byte{0xc1}, std::byte{0xe0}, std::byte{0x09}, std::byte{0x81}, std::byte{0xe2}, std::byte{0xff},
	std::byte{0xfc}, std::byte{0xff}, std::byte{0xff}, std::byte{0x0b}, std::byte{0xc2}, std::byte{0x0f}, std::byte{0xba}, std::byte{0xf0}, std::byte{0x0a}, std::byte{0x0b}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d}, std::byte{0x20}, std::byte{0x02},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x85}, std::byte{0x88}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x75}, std::byte{0x07}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x86}, std::byte{0x28}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x89}, std::byte{0x85}, std::byte{0x28}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x89}, std::byte{0xb5}, std::byte{0x30},
	std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0x38}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0x40}, std::byte{0x02}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0x48}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0x50}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48},
	std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0xe8}, std::byte{0x68}, std::byte{0x01}, std::byte{0x88}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x20}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10},
	std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x18}, std::byte{0x41}, std::byte{0xc6}, std::byte{0x85}, std::byte{0xa0}, std::byte{0x02},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0xff}, std::byte{0x05}, std::byte{0x3f}, std::byte{0xa8}, std::byte{0x5f}, std::byte{0x06}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0x38}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x85}, std::byte{0x90}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x9e}, std::byte{0x40}, std::byte{0x02}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x49}, std::byte{0x2b}, std::byte{0xdc}, std::byte{0x48}, std::byte{0xc1}, std::byte{0xfb}, std::byte{0x03}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x0f}, std::byte{0x8e}, std::byte{0xa4}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x49}, std::byte{0x8b}, std::byte{0x75}, std::byte{0x08}, std::byte{0x66}, std::byte{0x66}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x84}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0xfb}, std::byte{0x48}, std::byte{0xd1}, std::byte{0xef}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x04}, std::byte{0xfc}, std::byte{0x48}, std::byte{0x89}, std::byte{0x45}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x00}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xce}, std::byte{0x74}, std::byte{0x64}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x7e}, std::byte{0x10}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0xd6}, std::byte{0x48}, std::byte{0x83}, std::byte{0x7e}, std::byte{0x18}, std::byte{0x10}, std::byte{0x72}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x16}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x71}, std::byte{0x10}, std::byte{0x48},
	std::byte{0x83}, std::byte{0x79}, std::byte{0x18}, std::byte{0x10}, std::byte{0x72}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x09}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xc6}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xfe}, std::byte{0x4d},
	std::byte{0x0f}, std::byte{0x42}, std::byte{0xc7}, std::byte{0xe8}, std::byte{0xf8}, std::byte{0x3d}, std::byte{0xbb}, std::byte{0x00}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x75}, std::byte{0x14}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xf7}, std::byte{0x73},
	std::byte{0x07}, std::byte{0xb8}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xeb}, std::byte{0x08}, std::byte{0x33}, std::byte{0xc0}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xf7}, std::byte{0x0f}, std::byte{0x97}, std::byte{0xc0},
	std::byte{0xc1}, std::byte{0xe8}, std::byte{0x1f}, std::byte{0x84}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x17}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x65}, std::byte{0x78}, std::byte{0x49}, std::byte{0x83}, std::byte{0xc4}, std::byte{0x08}, std::byte{0x48},
	std::byte{0xc7}, std::byte{0xc0}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x48}, std::byte{0x2b}, std::byte{0xc7}, std::byte{0x48}, std::byte{0x03}, std::byte{0xd8}, std::byte{0xeb}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0xdf}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x0f}, std::byte{0x8f}, std::byte{0x76}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x75}, std::byte{0x48}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x85}, std::byte{0x90}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x6d}, std::byte{0x48}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8e}, std::byte{0x40}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x3b}, std::byte{0x8e}, std::byte{0x48}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x74}, std::byte{0x38}, std::byte{0x4c}, std::byte{0x3b}, std::byte{0xe1}, std::byte{0x75}, std::byte{0x0d}, std::byte{0x4c}, std::byte{0x89},
	std::byte{0x29}, std::byte{0x48}, std::byte{0x83}, std::byte{0x86}, std::byte{0x40}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x08}, std::byte{0xeb}, std::byte{0x35}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x41}, std::byte{0xf8}, std::byte{0x49},
	std::byte{0x8b}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x01}, std::byte{0x48}, std::byte{0x83}, std::byte{0x86}, std::byte{0x40}, std::byte{0x02}, std::byte{0x00}, std::byte{0x00}, std::byte{0x08}, std::byte{0x4d}, std::byte{0x2b}, std::byte{0xc4},
	std::byte{0x49}, std::byte{0x2b}, std::byte{0xc8}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xd4}, std::byte{0xe8}, std::byte{0x4d}, std::byte{0x3d}, std::byte{0xbb}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0x2c}, std::byte{0x24}, std::byte{0xeb},
	std::byte{0x0f}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0x48}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xd4}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xc8}, std::byte{0xe8}, std::byte{0xe0}, std::byte{0x4d}, std::byte{0xed}, std::byte{0xf9},
	std::byte{0x45}, std::byte{0x33}, std::byte{0xc9}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0x70}, std::byte{0x33}, std::byte{0xd2}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x0d}, std::byte{0xb8}, std::byte{0xa7}, std::byte{0x5f}, std::byte{0x06},
	std::byte{0xff}, std::byte{0x15}, std::byte{0x0a}, std::byte{0x53}, std::byte{0x39}, std::byte{0x01}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x0f}, std::byte{0x84}, std::byte{0xdd}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x83}, std::byte{0x7d},
	std::byte{0x70}, std::byte{0x00}, std::byte{0x74}, std::byte{0x20}, std::byte{0xe8}, std::byte{0xa7}, std::byte{0x21}, std::byte{0x00}, std::byte{0x00}, std::byte{0x90}, std::byte{0x45}, std::byte{0x33}, std::byte{0xc0}, std::byte{0x33}, std::byte{0xd2}, std::byte{0x48},
	std::byte{0x8d}, std::byte{0x0d}, std::byte{0x92}, std::byte{0xa7}, std::byte{0x5f}, std::byte{0x06}, std::byte{0xff}, std::byte{0x15}, std::byte{0xcc}, std::byte{0x52}, std::byte{0x39}, std::byte{0x01}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x0f}, std::byte{0x84},
	std::byte{0xb7}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0xe8}, std::byte{0x87}, std::byte{0x21}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x58}, std::byte{0x08}, std::byte{0x45}, std::byte{0x33}, std::byte{0xc9},
	std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0x70}, std::byte{0x33}, std::byte{0xd2}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x0d}, std::byte{0x6b}, std::byte{0xa7}, std::byte{0x5f}, std::byte{0x06}, std::byte{0xff}, std::byte{0x15}, std::byte{0xbd},
	std::byte{0x52}, std::byte{0x39}, std::byte{0x01}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x0f}, std::byte{0x84}, std::byte{0x89}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x83}, std::byte{0x7d}, std::byte{0x70}, std::byte{0x00}, std::byte{0x74},
	std::byte{0x20}, std::byte{0xe8}, std::byte{0x5a}, std::byte{0x21}, std::byte{0x00}, std::byte{0x00}, std::byte{0x90}, std::byte{0x45}, std::byte{0x33}, std::byte{0xc0}, std::byte{0x33}, std::byte{0xd2}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x0d}, std::byte{0x45},
	std::byte{0xa7}, std::byte{0x5f}, std::byte{0x06}, std::byte{0xff}, std::byte{0x15}, std::byte{0x7f}, std::byte{0x52}, std::byte{0x39}, std::byte{0x01}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x0f}, std::byte{0x84}, std::byte{0x63}, std::byte{0x01}, std::byte{0x00},
	std::byte{0x00}, std::byte{0xe8}, std::byte{0x3a}, std::byte{0x21}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x20}, std::byte{0x49}, std::byte{0x2b}, std::byte{0xdc}, std::byte{0x48}, std::byte{0xc1}, std::byte{0xfb}, std::byte{0x03},
	std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x0f}, std::byte{0x8e}, std::byte{0x91}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x8b}, std::byte{0x75}, std::byte{0x08}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0xfb}, std::byte{0x48}, std::byte{0xd1}, std::byte{0xef}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x04}, std::byte{0xfc}, std::byte{0x48}, std::byte{0x89}, std::byte{0x45}, std::byte{0x48}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x00}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xce}, std::byte{0x74}, std::byte{0x64}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x7e}, std::byte{0x10}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0xd6}, std::byte{0x48}, std::byte{0x83}, std::byte{0x7e}, std::byte{0x18}, std::byte{0x10}, std::byte{0x72}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x16}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x71}, std::byte{0x10}, std::byte{0x48},
	std::byte{0x83}, std::byte{0x79}, std::byte{0x18}, std::byte{0x10}, std::byte{0x72}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x09}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xc6}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xfe}, std::byte{0x4d},
	std::byte{0x0f}, std::byte{0x42}, std::byte{0xc7}, std::byte{0xe8}, std::byte{0x58}, std::byte{0x3c}, std::byte{0xbb}, std::byte{0x00}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x75}, std::byte{0x14}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xf7}, std::byte{0x73},
	std::byte{0x07}, std::byte{0xb8}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xeb}, std::byte{0x08}, std::byte{0x33}, std::byte{0xc0}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xf7}, std::byte{0x0f}, std::byte{0x97}, std::byte{0xc0},
	std::byte{0xc1}, std::byte{0xe8}, std::byte{0x1f}, std::byte{0x84}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x17}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x65}, std::byte{0x48}, std::byte{0x49}, std::byte{0x83}, std::byte{0xc4}, std::byte{0x08}, std::byte{0x48},
	std::byte{0xc7}, std::byte{0xc0}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x48}, std::byte{0x2b}, std::byte{0xc7}, std::byte{0x48}, std::byte{0x03}, std::byte{0xd8}, std::byte{0xeb}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0xdf}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x0f}, std::byte{0x8f}, std::byte{0x76}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x45}, std::byte{0x33}, std::byte{0xc9}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45},
	std::byte{0x70}, std::byte{0x33}, std::byte{0xd2}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x0d}, std::byte{0x7e}, std::byte{0xa6}, std::byte{0x5f}, std::byte{0x06}, std::byte{0xff}, std::byte{0x15}, std::byte{0xd0}, std::byte{0x51}, std::byte{0x39}, std::byte{0x01},
	std::byte{0x85}, std::byte{0xc0}, std::byte{0x0f}, std::byte{0x84}, std::byte{0x95}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x83}, std::byte{0x7d}, std::byte{0x70}, std::byte{0x00}, std::byte{0x74}, std::byte{0x1c}, std::byte{0xe8}, std::byte{0x6d},
	std::byte{0x20}, std::byte{0x00}, std::byte{0x00}, std::byte{0x90}, std::byte{0x45}, std::byte{0x33}, std::byte{0xc0}, std::byte{0x33}, std::byte{0xd2}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x0d}, std::byte{0x58}, std::byte{0xa6}, std::byte{0x5f}, std::byte{0x06},
	std::byte{0xff}, std::byte{0x15}, std::byte{0x92}, std::byte{0x51}, std::byte{0x39}, std::byte{0x01}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x73}, std::byte{0xe8}, std::byte{0x51}, std::byte{0x20}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4c},
	std::byte{0x8b}, std::byte{0xc8}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x6d}, std::byte{0x48}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x48}, std::byte{0x10}, std::byte{0x74}, std::byte{0x32},
	std::byte{0x4c}, std::byte{0x3b}, std::byte{0xe1}, std::byte{0x75}, std::byte{0x0a}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x29}, std::byte{0x48}, std::byte{0x83}, std::byte{0x40}, std::byte{0x08}, std::byte{0x08}, std::byte{0xeb}, std::byte{0x33}, std::byte{0x4c},
	std::byte{0x8d}, std::byte{0x41}, std::byte{0xf8}, std::byte{0x49}, std::byte{0x8b}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x01}, std::byte{0x49}, std::byte{0x83}, std::byte{0x41}, std::byte{0x08}, std::byte{0x08}, std::byte{0x4d}, std::byte{0x2b},
	std::byte{0xc4}, std::byte{0x49}, std::byte{0x2b}, std::byte{0xc8}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xd4}, std::byte{0xe8}, std::byte{0x7c}, std::byte{0x3b}, std::byte{0xbb}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0x2c}, std::byte{0x24},
	std::byte{0xeb}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0x48}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xd4}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xc9}, std::byte{0xe8}, std::byte{0x0f}, std::byte{0x4c}, std::byte{0xed},
	std::byte{0xf9}, std::byte{0x90}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xc5}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x9c}, std::byte{0x24}, std::byte{0xc0}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc4},
	std::byte{0x70}, std::byte{0x41}, std::byte{0x5f}, std::byte{0x41}, std::byte{0x5e}, std::byte{0x41}, std::byte{0x5d}, std::byte{0x41}, std::byte{0x5c}, std::byte{0x5f}, std::byte{0x5e}, std::byte{0x5d}, std::byte{0xc3}, std::byte{0xff}, std::byte{0x15}, std::byte{0x35},
	std::byte{0x56}, std::byte{0x39}, std::byte{0x01}, std::byte{0x90}, std::byte{0xff}, std::byte{0x15}, std::byte{0x2e}, std::byte{0x56}, std::byte{0x39}, std::byte{0x01}, std::byte{0xcc}, std::byte{0xff}, std::byte{0x15}, std::byte{0x27}, std::byte{0x56}, std::byte{0x39},
	std::byte{0x01}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5c}, std::byte{0x24}, std::byte{0x08}, std::byte{0x57}, std::byte{0x48}, std::byte{0x83}, std::byte{0xec}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xd9}, std::byte{0x33}, std::byte{0xff}, std::byte{0x48},
	std::byte{0x8b}, std::byte{0x49}, std::byte{0x38}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc9}, std::byte{0x74}, std::byte{0x3d}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x48}, std::byte{0x48}, std::byte{0x2b}, std::byte{0xd1}, std::byte{0x48},
	std::byte{0x83}, std::byte{0xe2}, std::byte{0xf8}, std::byte{0x48}, std::byte{0x81}, std::byte{0xfa}, std::byte{0x00}, std::byte{0x10}, std::byte{0x00}, std::byte{0x00}, std::byte{0x72}, std::byte{0x18}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x41}, std::byte{0xf8},
	std::byte{0x48}, std::byte{0x83}, std::byte{0xc2}, std::byte{0x27}, std::byte{0x49}, std::byte{0x2b}, std::byte{0xc8}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x41}, std::byte{0xf8}, std::byte{0x48}, std::byte{0x83}, std::byte{0xf8}, std::byte{0x1f}, std::byte{0x77},
	std::byte{0x63}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xc8}, std::byte{0xe8}, std::byte{0x17}, std::byte{0x9c}, std::byte{0x83}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x48}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x40}, std::byte{0x48}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x48}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0b}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc9}, std::byte{0x74}, std::byte{0x3c}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53},
	std::byte{0x10}, std::byte{0x48}, std::byte{0x2b}, std::byte{0xd1}, std::byte{0x48}, std::byte{0x83}, std::byte{0xe2}, std::byte{0xf0}, std::byte{0x48}, std::byte{0x81}, std::byte{0xfa}, std::byte{0x00}, std::byte{0x10}, std::byte{0x00}, std::byte{0x00}, std::byte{0x72},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x41}, std::byte{0xf8}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc2}, std::byte{0x27}, std::byte{0x49}, std::byte{0x2b}, std::byte{0xc8}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x41}, std::byte{0xf8},
	std::byte{0x48}, std::byte{0x83}, std::byte{0xf8}, std::byte{0x1f}, std::byte{0x77}, std::byte{0x1e}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xc8}, std::byte{0xe8}, std::byte{0xd2}, std::byte{0x9b}, std::byte{0x83}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89},
	std::byte{0x3b}, std::byte{0x48}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x5c}, std::byte{0x24}, std::byte{0x30}, std::byte{0x48}, std::byte{0x83},
	std::byte{0xc4}, std::byte{0x20}, std::byte{0x5f}, std::byte{0xc3}, std::byte{0xff}, std::byte{0x15}, std::byte{0x86}, std::byte{0x55}, std::byte{0x39}, std::byte{0x01}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc},
	std::byte{0x80}, std::byte{0x79}, std::byte{0x28}, std::byte{0x00}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x44}, std::byte{0x8b}, std::byte{0x41}, std::byte{0x04}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x51}, std::byte{0x08}, std::byte{0xe9}, std::byte{0xed},
	std::byte{0xbd}, std::byte{0xf3}, std::byte{0xf9}, std::byte{0xc3}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc}, std::byte{0xcc},
};

constexpr std::array<std::byte, 1465> studio_0732_constructor_fixture = {
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5c}, std::byte{0x24}, std::byte{0x18}, std::byte{0x48}, std::byte{0x89}, std::byte{0x54}, std::byte{0x24}, std::byte{0x10}, std::byte{0x48}, std::byte{0x89}, std::byte{0x4c}, std::byte{0x24}, std::byte{0x08}, std::byte{0x55},
	std::byte{0x56}, std::byte{0x57}, std::byte{0x41}, std::byte{0x54}, std::byte{0x41}, std::byte{0x55}, std::byte{0x41}, std::byte{0x56}, std::byte{0x41}, std::byte{0x57}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xec}, std::byte{0x48}, std::byte{0x83}, std::byte{0xec},
	std::byte{0x70}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xd0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xf2}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0xe9}, std::byte{0x8b}, std::byte{0x45}, std::byte{0x60}, std::byte{0x89}, std::byte{0x44}, std::byte{0x24},
	std::byte{0x20}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x75}, std::byte{0x78}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xc6}, std::byte{0x49}, std::byte{0x8b}, std::byte{0xd2}, std::byte{0xe8}, std::byte{0x80}, std::byte{0xa1}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x8d}, std::byte{0x05}, std::byte{0x91}, std::byte{0xec}, std::byte{0x7e}, std::byte{0x03}, std::byte{0x49}, std::byte{0x89}, std::byte{0x45}, std::byte{0x00}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x5d}, std::byte{0x28}, std::byte{0x48},
	std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0x90}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29}, std::byte{0x45},
	std::byte{0xc0}, std::byte{0x45}, std::byte{0x33}, std::byte{0xff}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x7b}, std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d},
	std::byte{0x46}, std::byte{0x28}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0x44}, std::byte{0x88}, std::byte{0x7b}, std::byte{0x40}, std::byte{0x66}, std::byte{0x0f},
	std::byte{0x73}, std::byte{0xd8}, std::byte{0x08}, std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53},
	std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43},
	std::byte{0x08}, std::byte{0x10}, std::byte{0xeb}, std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0x50}, std::byte{0x41}, std::byte{0x9d}, std::byte{0xfa},
	std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x21}, std::byte{0x48},
	std::byte{0x8b}, std::byte{0x0f}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x84}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xca}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x5d}, std::byte{0x70}, std::byte{0x48}, std::byte{0x89}, std::byte{0x5d},
	std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0x98}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c},
	std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x18}, std::byte{0x4c}, std::byte{0x89},
	std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x46}, std::byte{0x70}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43},
	std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08}, std::byte{0x66}, std::byte{0x48},
	std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x53}, std::byte{0x10},
	std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83}, std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb}, std::byte{0x0c}, std::byte{0x4c},
	std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0xa5}, std::byte{0x40}, std::byte{0x9d}, std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x66}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x14}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x90},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xd7}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0xb8}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0xa0}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0xb8},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66},
	std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08}, std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83},
	std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb}, std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0xff}, std::byte{0x3f}, std::byte{0x9d},
	std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x20},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x84}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xcb}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0x00}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0xa8}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0x00},
	std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66},
	std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08}, std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83},
	std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb}, std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0x4f}, std::byte{0x3f}, std::byte{0x9d},
	std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x20},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x84}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xcb}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0x48}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x8d}, std::byte{0xb0}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x29},
	std::byte{0x45}, std::byte{0xc0}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b},
	std::byte{0x18}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x7b}, std::byte{0x28}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0x48},
	std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x30}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x38}, std::byte{0xc6}, std::byte{0x43}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66},
	std::byte{0x0f}, std::byte{0x73}, std::byte{0xd8}, std::byte{0x08}, std::byte{0x66}, std::byte{0x48}, std::byte{0x0f}, std::byte{0x7e}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x23}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0x53}, std::byte{0x10}, std::byte{0x74}, std::byte{0x0d}, std::byte{0x0f}, std::byte{0x10}, std::byte{0x01}, std::byte{0x0f}, std::byte{0x11}, std::byte{0x02}, std::byte{0x48}, std::byte{0x83},
	std::byte{0x43}, std::byte{0x08}, std::byte{0x10}, std::byte{0xeb}, std::byte{0x0c}, std::byte{0x4c}, std::byte{0x8d}, std::byte{0x45}, std::byte{0xc0}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xcb}, std::byte{0xe8}, std::byte{0x9f}, std::byte{0x3e}, std::byte{0x9d},
	std::byte{0xfa}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x3f}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x53}, std::byte{0x08}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x03}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x74}, std::byte{0x20},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x0f}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x40}, std::byte{0x00}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x84}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x03}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x89}, std::byte{0x0f}, std::byte{0x48}, std::byte{0x83}, std::byte{0xc0}, std::byte{0x10}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xc2}, std::byte{0x75}, std::byte{0xf0},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0x5b}, std::byte{0x30}, std::byte{0x48}, std::byte{0x85}, std::byte{0xdb}, std::byte{0x75}, std::byte{0xcb}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x86}, std::byte{0x90}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x25}, std::byte{0x00}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x48}, std::byte{0x0b}, std::byte{0x85}, std::byte{0x80}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x89}, std::byte{0x85},
	std::byte{0x90}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0x98}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0xa0}, std::byte{0x01},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x85}, std::byte{0x88}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x75}, std::byte{0x07}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x86}, std::byte{0xa8}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x89}, std::byte{0x85}, std::byte{0xa8}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x89}, std::byte{0xb5}, std::byte{0xb0},
	std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x85}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x33}, std::byte{0x46}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe0},
	std::byte{0x01}, std::byte{0x41}, std::byte{0x31}, std::byte{0x85}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x85}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41},
	std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0xd1}, std::byte{0xf9}, std::byte{0x03}, std::byte{0xc9}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x06}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89},
	std::byte{0x8d}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x46}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe0}, std::byte{0xf8}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x83}, std::byte{0xe0},
	std::byte{0x08}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x41}, std::byte{0x89}, std::byte{0x85}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe1},
	std::byte{0xf0}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x10}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41},
	std::byte{0x8b}, std::byte{0x46}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe0}, std::byte{0xe0}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x83}, std::byte{0xe0}, std::byte{0x20}, std::byte{0x33}, std::byte{0xc1}, std::byte{0x41}, std::byte{0x89}, std::byte{0x85},
	std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe1}, std::byte{0xc0}, std::byte{0x33}, std::byte{0xc8}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x40},
	std::byte{0x33}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x56}, std::byte{0x10}, std::byte{0x83}, std::byte{0xe2}, std::byte{0x80},
	std::byte{0x33}, std::byte{0xd1}, std::byte{0x81}, std::byte{0xe2}, std::byte{0x80}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x33}, std::byte{0xd1}, std::byte{0x41}, std::byte{0x89}, std::byte{0x95}, std::byte{0xbc}, std::byte{0x01}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x41}, std::byte{0x8b}, std::byte{0x4e}, std::byte{0x10}, std::byte{0xc1}, std::byte{0xf9}, std::byte{0x08}, std::byte{0x83}, std::byte{0xe1}, std::byte{0x01}, std::byte{0x0f}, std::byte{0xb6}, std::byte{0x45}, std::byte{0x70}, std::byte{0xc1},
	std::byte{0xe0}, std::byte{0x02}, std::byte{0x0b}, std::byte{0xc8}, std::byte{0xc1}, std::byte{0xe1}, std::byte{0x08}, std::byte{0x0f}, std::byte{0xb6}, std::byte{0x45}, std::byte{0x68}, std::byte{0xc1}, std::byte{0xe0}, std::byte{0x09}, std::byte{0x81}, std::byte{0xe2},
	std::byte{0xff}, std::byte{0xfc}, std::byte{0xff}, std::byte{0xff}, std::byte{0x0b}, std::byte{0xc2}, std::byte{0x0f}, std::byte{0xba}, std::byte{0xf0}, std::byte{0x0a}, std::byte{0x0b}, std::byte{0xc8}, std::byte{0x41}, std::byte{0x89}, std::byte{0x8d}, std::byte{0xbc},
	std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0xc0}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0xc8}, std::byte{0x01}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x4d}, std::byte{0x89}, std::byte{0xbd}, std::byte{0xd0}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x9d}, std::byte{0xd8}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00}, std::byte{0x48},
	std::byte{0x89}, std::byte{0x5d}, std::byte{0x78}, std::byte{0xe8}, std::byte{0x38}, std::byte{0x7e}, std::byte{0x88}, std::byte{0x00}, std::byte{0x48}, std::byte{0x89}, std::byte{0x43}, std::byte{0x20}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x10},
	std::byte{0x4c}, std::byte{0x89}, std::byte{0x3b}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x08}, std::byte{0x4c}, std::byte{0x89}, std::byte{0x7b}, std::byte{0x18}, std::byte{0x41}, std::byte{0xc6}, std::byte{0x85}, std::byte{0x28}, std::byte{0x02},
	std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0xff}, std::byte{0x05}, std::byte{0x1f}, std::byte{0xf2}, std::byte{0x60}, std::byte{0x06}, std::byte{0x48}, std::byte{0x8d}, std::byte{0x86}, std::byte{0xc0}, std::byte{0x01}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x89}, std::byte{0x85}, std::byte{0x90}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x20}, std::byte{0x48}, std::byte{0x8b}, std::byte{0xbe}, std::byte{0xc8}, std::byte{0x01}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x49}, std::byte{0x2b}, std::byte{0xfc}, std::byte{0x48}, std::byte{0xc1}, std::byte{0xff}, std::byte{0x03}, std::byte{0x48}, std::byte{0x85}, std::byte{0xff}, std::byte{0x0f}, std::byte{0x8e}, std::byte{0xa4}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x00}, std::byte{0x49}, std::byte{0x8b}, std::byte{0x75}, std::byte{0x08}, std::byte{0x66}, std::byte{0x66}, std::byte{0x66}, std::byte{0x0f}, std::byte{0x1f}, std::byte{0x84}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	std::byte{0x48}, std::byte{0x8b}, std::byte{0xdf}, std::byte{0x48}, std::byte{0xd1}, std::byte{0xeb}, std::byte{0x49}, std::byte{0x8d}, std::byte{0x04}, std::byte{0xdc}, std::byte{0x48}, std::byte{0x89}, std::byte{0x45}, std::byte{0x78}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0x00}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x48}, std::byte{0x08}, std::byte{0x48}, std::byte{0x3b}, std::byte{0xce}, std::byte{0x74}, std::byte{0x64}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x7e}, std::byte{0x10}, std::byte{0x48}, std::byte{0x8b},
	std::byte{0xd6}, std::byte{0x48}, std::byte{0x83}, std::byte{0x7e}, std::byte{0x18}, std::byte{0x10}, std::byte{0x72}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x16}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x71}, std::byte{0x10}, std::byte{0x48},
	std::byte{0x83}, std::byte{0x79}, std::byte{0x18}, std::byte{0x10}, std::byte{0x72}, std::byte{0x03}, std::byte{0x48}, std::byte{0x8b}, std::byte{0x09}, std::byte{0x4d}, std::byte{0x8b}, std::byte{0xc6}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xfe}, std::byte{0x4d},
	std::byte{0x0f}, std::byte{0x42}, std::byte{0xc7}, std::byte{0xe8}, std::byte{0xf8}, std::byte{0xba}, std::byte{0xbb}, std::byte{0x00}, std::byte{0x85}, std::byte{0xc0}, std::byte{0x75}, std::byte{0x14}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xf7}, std::byte{0x73},
	std::byte{0x07}, std::byte{0xb8}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xeb}, std::byte{0x08}, std::byte{0x33}, std::byte{0xc0}, std::byte{0x4d}, std::byte{0x3b}, std::byte{0xf7}, std::byte{0x0f}, std::byte{0x97}, std::byte{0xc0},
	std::byte{0xc1}, std::byte{0xe8}, std::byte{0x1f}, std::byte{0x84}, std::byte{0xc0}, std::byte{0x74}, std::byte{0x17}, std::byte{0x4c}, std::byte{0x8b}, std::byte{0x65}, std::byte{0x78}, std::byte{0x49}, std::byte{0x83}, std::byte{0xc4}, std::byte{0x08}, std::byte{0x48},
	std::byte{0xc7}, std::byte{0xc0}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0xff}, std::byte{0x48}, std::byte{0x2b}, std::byte{0xc3},
};


#pragma pack(push, 1)
	struct RuntimeFunctionEntryRaw
	{
		std::uint32_t begin_address;
		std::uint32_t end_address;
		std::uint32_t unwind_info_address;
	};
#pragma pack(pop)

	template <std::size_t N>
	auto resolve_all(
		const std::array<std::byte, N>& code,
		const std::size_t size,
		const rml::roblox::internals::ReflectionVftSets& vft_sets,
		const std::uintptr_t atom_addr = get_string_atom_target)
	{
		std::vector<RuntimeFunctionEntryRaw> entries;
		ZydisDecoder decoder;
		if (!ZYAN_SUCCESS(ZydisDecoderInit(
				&decoder, ZYDIS_MACHINE_MODE_LONG_64, ZYDIS_STACK_WIDTH_64)))
			std::abort();
		std::uint32_t begin = 0;
		std::uint32_t offset = 0;
		while (offset < size)
		{
			ZydisDecodedInstruction instruction{};
			ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT]{};
			const auto* bytes = reinterpret_cast<const std::uint8_t*>(code.data() + offset);
			if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
					&decoder, bytes, size - offset, &instruction, operands)) ||
				instruction.length == 0)
				std::abort();
			offset += instruction.length;
			if (instruction.mnemonic != ZYDIS_MNEMONIC_RET)
				continue;
			entries.push_back(RuntimeFunctionEntryRaw{
				.begin_address = begin,
				.end_address = offset,
				.unwind_info_address = 1,
			});
			begin = offset;
		}
		if (begin < size)
		{
			entries.push_back(RuntimeFunctionEntryRaw{
				.begin_address = begin,
				.end_address = static_cast<std::uint32_t>(size),
				.unwind_info_address = 1,
			});
		}
		const auto pdata_span = std::as_bytes(std::span{entries});
		return resolve_reflection_layout(std::span{code}.first(size), image_base, atom_addr, pdata_span, image_base, vft_sets);
	}

	template <std::size_t N>
	auto resolve(
		const std::array<std::byte, N>& code,
		const std::size_t size,
		const std::uintptr_t class_vft = vft_class_descriptor)
	{
		const std::array<std::uintptr_t, 1> class_vfts{class_vft};

		const rml::roblox::internals::ReflectionVftSets vft_sets{
			.class_descriptor_vfts = class_vfts,
		};
		const RuntimeFunctionEntryRaw entry{
			.begin_address = 0,
			.end_address = static_cast<std::uint32_t>(size),
			.unwind_info_address = 1,
		};
		return resolve_reflection_layout(
			std::span{code}.first(size),
			image_base,
			get_string_atom_target,
			std::as_bytes(std::span{&entry, 1}),
			image_base,
			vft_sets);
	}
	struct MockDescriptor
	{
		void* vft{nullptr};
		const RBX::Name* name_ptr{nullptr};
		void* pad1{nullptr};
		void* pad2{nullptr};
		std::uint64_t attributes{0};
	};

	struct CollectionEntry
	{
		const void* descriptor;
		std::uint64_t unk;
	};

	struct VectorTriple
	{
		std::uintptr_t my_first;
		std::uintptr_t my_last;
		std::uintptr_t my_end;
	};
}

const rml::roblox::internals::RobloxInternalsProfile& get_roblox_internals_profile()
{
	std::abort();
}

const rml::roblox::internals::RobloxInternalsProfile* try_get_roblox_internals_profile() noexcept
{
	return nullptr;
}

int main(const int argc, char** argv)
{
	const std::array<std::uintptr_t, 1> desc_vfts{vft_descriptor};
	const std::array<std::uintptr_t, 1> mem_vfts{vft_member};
	const std::array<std::uintptr_t, 1> prop_vfts{vft_property};
	const std::array<std::uintptr_t, 1> func_vfts{vft_function};
	const std::array<std::uintptr_t, 1> yield_vfts{vft_yield_function};
	const std::array<std::uintptr_t, 1> event_vfts{vft_event};
	const std::array<std::uintptr_t, 1> callback_vfts{vft_callback};
	const std::array<std::uintptr_t, 1> class_vfts{vft_class_descriptor};

	const rml::roblox::internals::ReflectionVftSets full_vft_sets{
		.descriptor_vfts = desc_vfts,
		.member_vfts = mem_vfts,
		.property_vfts = prop_vfts,
		.function_vfts = func_vfts,
		.yield_function_vfts = yield_vfts,
		.event_vfts = event_vfts,
		.callback_vfts = callback_vfts,
		.class_descriptor_vfts = class_vfts,
	};

	auto build_full_code = [](
		std::array<std::byte, 4096>& code,
		std::size_t& size,
		std::uintptr_t get_atom_addr,
		const std::array<std::uint32_t, 5>& containers,
		std::uint32_t base_off,
		std::uint32_t func_off,
		std::uint32_t name_off = 0x8,
		std::uint32_t owner_off = 0x30,
		std::uint32_t sec_off = 0x38,
		std::uint32_t prop_type_off = 0x40,
		std::uint32_t prop_func_off = 0x8c,
		std::uint32_t sig_off = 0x48,
		std::uint32_t fkind_off = 0x78,
		std::uint32_t finv_off = 0x80,
		std::uint32_t fdelta_off = 0x88,
		std::uint32_t csig_off = 0x40,
		std::uint32_t casync_off = 0x78,
		std::uint32_t esig_off = 0x78)
	{
		size = 0;
		// 1. Descriptor constructor: LEA RAX, [vft_descriptor]; MOV [RCX], RAX; CALL get_atom_addr; MOV [RCX + name_off], RAX; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		auto inst_addr = image_base + size - 3;
		auto disp = static_cast<std::int32_t>(vft_descriptor - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0xe8};
		inst_addr = image_base + size - 1;
		disp = static_cast<std::int32_t>(get_atom_addr - (inst_addr + 5));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x81};
		append_u32(code, size, name_off);
		code[size++] = std::byte{0xc3};

		// 2. MemberDescriptor constructor: LEA RAX, [vft_member]; MOV [RCX], RAX; MOV [RCX + owner_off], RDX; MOV [RCX + sec_off], R9; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_member - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x91};
		append_u32(code, size, owner_off);
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x89};
		append_u32(code, size, sec_off);
		code[size++] = std::byte{0xc3};

		// 3. PropertyDescriptor constructor: LEA RAX, [vft_property]; MOV [RCX], RAX; MOV [RCX + prop_type_off], R8; TEST dword ptr [RCX + prop_func_off], 1; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_property - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x81};
		append_u32(code, size, prop_type_off);
		code[size++] = std::byte{0xf7}; code[size++] = std::byte{0x81};
		append_u32(code, size, prop_func_off);
		append_u32(code, size, 0x01);
		code[size++] = std::byte{0xc3};

		// 4. FunctionDescriptor constructor: LEA RAX, [vft_function]; MOV [RCX], RAX; MOV [RCX + sig_off], R9; MOV dword ptr [RCX + fkind_off], 1; LEA R8, [RIP + 0x80]; MOV [RCX + finv_off], R8; MOV qword ptr [RCX + fdelta_off], 0; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_function - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x89};
		append_u32(code, size, sig_off);
		code[size++] = std::byte{0xc7}; code[size++] = std::byte{0x81};
		append_u32(code, size, fkind_off);
		append_u32(code, size, 1);
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		append_u32(code, size, 0x80);
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x81};
		append_u32(code, size, finv_off);
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0xc7}; code[size++] = std::byte{0x81};
		append_u32(code, size, fdelta_off);
		append_u32(code, size, 0);
		code[size++] = std::byte{0xc3};

		// 5. YieldFunctionDescriptor constructor: LEA RAX, [vft_yield_function]; MOV [RCX], RAX; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_yield_function - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0xc3};

		// 6. CallbackDescriptor constructor: LEA RAX, [vft_callback]; MOV [RCX], RAX; MOV [RCX + csig_off], R9; MOV byte ptr [RCX + casync_off], 0; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_callback - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x89};
		append_u32(code, size, csig_off);
		code[size++] = std::byte{0xc6}; code[size++] = std::byte{0x81};
		append_u32(code, size, casync_off);
		code[size++] = std::byte{0};
		code[size++] = std::byte{0xc3};

		// 7. EventDescriptor constructor: LEA RAX, [vft_event]; MOV [RCX], RAX; LEA R8, [RCX + esig_off]; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_event - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x81};
		append_u32(code, size, esig_off);
		code[size++] = std::byte{0xc3};

		// 8. ClassDescriptor constructor: LEA RAX, [vft_class_descriptor]; MOV [RCX], RAX; XOR R9, R9; ... containers, base, functionality
		append_constructor_code(code, size, containers, base_off, func_off);
	};
	auto build_full_code_msvc = [](
		std::array<std::byte, 4096>& code,
		std::size_t& size,
		std::uintptr_t get_atom_addr,
		const std::array<std::uint32_t, 5>& containers,
		std::uint32_t base_off,
		std::uint32_t func_off,
		std::uint32_t name_off = 0x8,
		std::uint32_t owner_off = 0x30,
		std::uint32_t sec_off = 0x38,
		std::uint32_t prop_type_off = 0x40,
		std::uint32_t prop_func_off = 0x8c,
		std::uint32_t sig_off = 0x48,
		std::uint32_t fkind_off = 0x78,
		std::uint32_t finv_off = 0x60,
		std::uint32_t fdelta_off = 0x68,
		std::uint32_t csig_off = 0x48,
		std::uint32_t casync_off = 0x78,
		std::uint32_t esig_off = 0x48)
	{
		size = 0;
		// 1. Descriptor constructor: LEA RAX, [vft_descriptor]; MOV [RCX], RAX; CALL get_atom_addr; MOV [RCX + name_off], RAX; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		auto inst_addr = image_base + size - 3;
		auto disp = static_cast<std::int32_t>(vft_descriptor - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0xe8};
		inst_addr = image_base + size - 1;
		disp = static_cast<std::int32_t>(get_atom_addr - (inst_addr + 5));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x81};
		append_u32(code, size, name_off);
		code[size++] = std::byte{0xc3};

		// 2. MemberDescriptor constructor: LEA RAX, [vft_member]; MOV [RCX], RAX; MOV [RCX + owner_off], RDX; MOV [RCX + sec_off], R9; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_member - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x91};
		append_u32(code, size, owner_off);
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x89};
		append_u32(code, size, sec_off);
		code[size++] = std::byte{0xc3};

		// 3. PropertyDescriptor constructor: LEA RAX, [vft_property]; MOV [RCX], RAX; MOV [RCX + prop_type_off], R8; TEST dword ptr [RCX + prop_func_off], 1; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_property - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x81};
		append_u32(code, size, prop_type_off);
		code[size++] = std::byte{0xf7}; code[size++] = std::byte{0x81};
		append_u32(code, size, prop_func_off);
		append_u32(code, size, 0x01);
		code[size++] = std::byte{0xc3};

		// 4. FunctionDescriptor constructor, alternate MSVC register allocation.
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_function - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x89};
		append_u32(code, size, sig_off);
		code[size++] = std::byte{0xc7}; code[size++] = std::byte{0x81};
		append_u32(code, size, fkind_off);
		append_u32(code, size, 1);
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x15};
		append_u32(code, size, 0x40);
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x91};
		append_u32(code, size, finv_off);
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0xc7}; code[size++] = std::byte{0x81};
		append_u32(code, size, fdelta_off);
		append_u32(code, size, 0);
		code[size++] = std::byte{0xc3};

		// 5. YieldFunctionDescriptor constructor: LEA RAX, [vft_yield_function]; MOV [RCX], RAX; RET
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_yield_function - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0xc3};

		// 6. CallbackDescriptor constructor with direct argument stores.
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_callback - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x89};
		append_u32(code, size, csig_off);
		code[size++] = std::byte{0xc6}; code[size++] = std::byte{0x81};
		append_u32(code, size, casync_off);
		code[size++] = std::byte{0};
		code[size++] = std::byte{0xc3};

		// 7. EventDescriptor constructor: take the address of its embedded signal.
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x05};
		inst_addr = image_base + size - 3;
		disp = static_cast<std::int32_t>(vft_event - (inst_addr + 7));
		append_u32(code, size, static_cast<std::uint32_t>(disp));
		code[size++] = std::byte{0x48}; code[size++] = std::byte{0x89}; code[size++] = std::byte{0x01};
		code[size++] = std::byte{0x4c}; code[size++] = std::byte{0x8d}; code[size++] = std::byte{0x81};
		append_u32(code, size, esig_off);
		code[size++] = std::byte{0xc3};

		// 8. ClassDescriptor constructor: LEA RAX, [vft_class_descriptor]; MOV [RCX], RAX; XOR R9, R9; ...
		append_constructor_code(code, size, containers, base_off, func_off);
	};

	const std::array<std::uint32_t, 5> layout_0731_containers = {0x28, 0x88, 0xe8, 0x148, 0x1a8};
	const std::array<std::uint32_t, 5> layout_0732_containers = {0x28, 0x70, 0xb8, 0x100, 0x148};

	// 1. 0.731 positive encoded body test across all 8 families
	std::array<std::byte, 4096> code_0731{};
	std::size_t size_0731 = 0;
	build_full_code(code_0731, size_0731, get_string_atom_target, layout_0731_containers, 0x228, 0x220);

	auto layout_0731 = resolve_all(code_0731, size_0731, full_vft_sets);
	if (!layout_0731 ||
		layout_0731->name_offset != 0x8 ||
		layout_0731->descriptor_container_offsets != std::array<std::ptrdiff_t, 5>{0x28, 0x88, 0xe8, 0x148, 0x1a8} ||
		layout_0731->base_class_offset != 0x228 ||
		layout_0731->functionality_offset != 0x220 ||
		layout_0731->owner_offset != 0x30 ||
		layout_0731->security_offset != 0x38 ||
		layout_0731->property_type_offset != 0x40 ||
		layout_0731->property_functionality_offset != 0x8c ||
		layout_0731->signature_offset != 0x48 ||
		layout_0731->function_kind_offset != 0x78 ||
		layout_0731->function_invoke_func_ptr_offset != 0x80 ||
		layout_0731->function_bound_this_delta_offset != 0x88 ||
		layout_0731->callback_signature_offset != 0x40 ||
		layout_0731->callback_async_flag_offset != 0x78 ||
		layout_0731->event_signal_offset != 0x78)
	{
		if (!layout_0731)
		{
			std::cerr << "Test 1 failed: resolver error "
				<< static_cast<int>(layout_0731.error().failure)
				<< ", capability=" << layout_0731.error().capability
				<< ", matched=" << layout_0731.error().matched_calls << '\n';
		}
		else
		{
			std::cerr << "Test 1 failed: 0.731 layout mismatch: name=" << layout_0731->name_offset
				<< " base=" << layout_0731->base_class_offset
				<< " functionality=" << layout_0731->functionality_offset
				<< " owner=" << layout_0731->owner_offset
				<< " security=" << layout_0731->security_offset
				<< " property_type=" << layout_0731->property_type_offset
				<< " property_functionality=" << layout_0731->property_functionality_offset
				<< " signature=" << layout_0731->signature_offset
				<< " function_kind=" << layout_0731->function_kind_offset
				<< " invoke=" << layout_0731->function_invoke_func_ptr_offset
				<< " this_delta=" << layout_0731->function_bound_this_delta_offset
				<< " callback_signature=" << layout_0731->callback_signature_offset
				<< " callback_async=" << layout_0731->callback_async_flag_offset
				<< " event_signal=" << layout_0731->event_signal_offset << '\n';
		}
		return 1;
	}

	// 2. 0.732 positive encoded body test across all 8 families
	std::array<std::byte, 4096> code_0732{};
	std::size_t size_0732 = 0;
	build_full_code(code_0732, size_0732, get_string_atom_target, layout_0732_containers, 0x1a8, 0x1bc);

	auto layout_0732 = resolve_all(code_0732, size_0732, full_vft_sets);
	if (!layout_0732 ||
		layout_0732->name_offset != 0x8 ||
		layout_0732->descriptor_container_offsets != std::array<std::ptrdiff_t, 5>{0x28, 0x70, 0xb8, 0x100, 0x148} ||
		layout_0732->base_class_offset != 0x1a8 ||
		layout_0732->functionality_offset != 0x1bc ||
		layout_0732->owner_offset != 0x30 ||
		layout_0732->security_offset != 0x38 ||
		layout_0732->property_type_offset != 0x40 ||
		layout_0732->property_functionality_offset != 0x8c ||
		layout_0732->signature_offset != 0x48 ||
		layout_0732->function_kind_offset != 0x78 ||
		layout_0732->function_invoke_func_ptr_offset != 0x80 ||
		layout_0732->function_bound_this_delta_offset != 0x88 ||
		layout_0732->callback_signature_offset != 0x40 ||
		layout_0732->callback_async_flag_offset != 0x78 ||
		layout_0732->event_signal_offset != 0x78)
	{
		std::cerr << "Test 2 failed: 0.732 layout mismatch\n";
		return 2;
	}
	// 2b. MSVC PE encoded body test across all 8 families
	std::array<std::byte, 4096> code_msvc{};
	std::size_t size_msvc = 0;
	build_full_code_msvc(code_msvc, size_msvc, get_string_atom_target, layout_0731_containers, 0x228, 0x220);

	auto layout_msvc = resolve_all(code_msvc, size_msvc, full_vft_sets);
	if (!layout_msvc ||
		layout_msvc->signature_offset != 0x48 ||
		layout_msvc->function_kind_offset != 0x78 ||
		layout_msvc->function_invoke_func_ptr_offset != 0x60 ||
		layout_msvc->function_bound_this_delta_offset != 0x68 ||
		layout_msvc->callback_signature_offset != 0x48 ||
		layout_msvc->callback_async_flag_offset != 0x78 ||
		layout_msvc->event_signal_offset != 0x48)
	{
		if (!layout_msvc)
		{
			std::cerr << "Test 2b failed: resolver error "
				<< static_cast<int>(layout_msvc.error().failure)
				<< ", capability=" << layout_msvc.error().capability
				<< ", matched=" << layout_msvc.error().matched_calls << '\n';
		}
		else
		{
			std::cerr << "Test 2b failed: MSVC PE layout mismatch: signature=" << layout_msvc->signature_offset << " (exp 0x48)"
				<< " kind=" << layout_msvc->function_kind_offset << " (exp 0x78)"
				<< " invoke=" << layout_msvc->function_invoke_func_ptr_offset << " (exp 0x60)"
				<< " this_delta=" << layout_msvc->function_bound_this_delta_offset << " (exp 0x68)"
				<< " callback_sig=" << layout_msvc->callback_signature_offset << " (exp 0x48)"
				<< " callback_async=" << layout_msvc->callback_async_flag_offset << " (exp 0x78)"
				<< " event_sig=" << layout_msvc->event_signal_offset << " (exp 0x48)\n";
		}
		return 2;
	}

	// 3. Negative: Missing family VFT set
	rml::roblox::internals::ReflectionVftSets missing_family_vft_sets = full_vft_sets;
	missing_family_vft_sets.property_vfts = {};
	const auto missing_family_res = resolve_all(code_0731, size_0731, missing_family_vft_sets);
	if (missing_family_res || missing_family_res.error().failure != CompatibilityFailure::missing_signature)
	{
		std::cerr << "Test 3 failed: missing family VFT set check\n";
		return 3;
	}

	// 4. Negative: Wrong VFT (MOV instead of LEA or mismatched VFT address)
	std::array<std::byte, 4096> code_wrong_vft{};
	std::size_t size_wrong_vft = 0;
	build_full_code(code_wrong_vft, size_wrong_vft, get_string_atom_target, layout_0731_containers, 0x228, 0x220);

	rml::roblox::internals::ReflectionVftSets wrong_vft_sets = full_vft_sets;
	const std::array<std::uintptr_t, 1> wrong_desc_vft{vft_descriptor + 0x9900};
	wrong_vft_sets.descriptor_vfts = wrong_desc_vft;
	const auto wrong_vft_res = resolve_all(code_wrong_vft, size_wrong_vft, wrong_vft_sets);
	if (wrong_vft_res || wrong_vft_res.error().failure != CompatibilityFailure::insufficient_evidence)
	{
		std::cerr << "Test 4 failed: wrong VFT check\n";
		return 4;
	}

	// 5. Negative: Conflict (conflicting candidate evidence for owner_offset across two member constructors)
	std::array<std::byte, 4096> code_conflict{};
	std::size_t size_conflict = 0;
	build_full_code(code_conflict, size_conflict, get_string_atom_target, layout_0731_containers, 0x228, 0x220, 0x8, 0x30);
	// Append a second MemberDescriptor constructor with owner_offset = 0x50
	code_conflict[size_conflict++] = std::byte{0x48}; code_conflict[size_conflict++] = std::byte{0x8d}; code_conflict[size_conflict++] = std::byte{0x05};
	auto inst_addr_conf = image_base + size_conflict - 3;
	auto disp_conf = static_cast<std::int32_t>(vft_member - (inst_addr_conf + 7));
	append_u32(code_conflict, size_conflict, static_cast<std::uint32_t>(disp_conf));
	code_conflict[size_conflict++] = std::byte{0x48}; code_conflict[size_conflict++] = std::byte{0x89}; code_conflict[size_conflict++] = std::byte{0x01};
	code_conflict[size_conflict++] = std::byte{0x48}; code_conflict[size_conflict++] = std::byte{0x89}; code_conflict[size_conflict++] = std::byte{0x91};
	append_u32(code_conflict, size_conflict, 0x50);
	code_conflict[size_conflict++] = std::byte{0x4c}; code_conflict[size_conflict++] = std::byte{0x89}; code_conflict[size_conflict++] = std::byte{0x89};
	append_u32(code_conflict, size_conflict, 0x38);
	code_conflict[size_conflict++] = std::byte{0xc3};

	const auto conflict_res = resolve_all(code_conflict, size_conflict, full_vft_sets);
	if (conflict_res || conflict_res.error().failure != CompatibilityFailure::ambiguous_evidence)
	{
		std::cerr << "Test 5 failed: conflict check\n";
		return 5;
	}
	// Test ReflectionCapabilities find_* across families
	auto mock_get_string_atom = [](const char* name) -> std::uintptr_t {
		static const auto& prop_n = RBX::Name::declare("TestProperty");
		static const auto& event_n = RBX::Name::declare("TestEvent");
		static const auto& func_n = RBX::Name::declare("TestFunction");
		static const auto& yield_n = RBX::Name::declare("TestYieldFunction");
		static const auto& callback_n = RBX::Name::declare("TestCallback");
		if (std::strcmp(name, "TestProperty") == 0) return reinterpret_cast<std::uintptr_t>(&prop_n);
		if (std::strcmp(name, "TestEvent") == 0) return reinterpret_cast<std::uintptr_t>(&event_n);
		if (std::strcmp(name, "TestFunction") == 0) return reinterpret_cast<std::uintptr_t>(&func_n);
		if (std::strcmp(name, "TestYieldFunction") == 0) return reinterpret_cast<std::uintptr_t>(&yield_n);
		if (std::strcmp(name, "TestCallback") == 0) return reinterpret_cast<std::uintptr_t>(&callback_n);
		return 0;
	};

	const auto& prop_name = *reinterpret_cast<const RBX::Name*>(mock_get_string_atom("TestProperty"));
	const auto& event_name = *reinterpret_cast<const RBX::Name*>(mock_get_string_atom("TestEvent"));
	const auto& func_name = *reinterpret_cast<const RBX::Name*>(mock_get_string_atom("TestFunction"));
	const auto& yield_name = *reinterpret_cast<const RBX::Name*>(mock_get_string_atom("TestYieldFunction"));
	const auto& callback_name = *reinterpret_cast<const RBX::Name*>(mock_get_string_atom("TestCallback"));

	MockDescriptor mock_prop{nullptr, &prop_name};
	MockDescriptor mock_event{nullptr, &event_name};
	MockDescriptor mock_func{nullptr, &func_name};
	MockDescriptor mock_yield{nullptr, &yield_name};
	MockDescriptor mock_callback{nullptr, &callback_name};

	CollectionEntry prop_entries[1] = {{&mock_prop, 0}};
	CollectionEntry event_entries[1] = {{&mock_event, 0}};
	CollectionEntry func_entries[1] = {{&mock_func, 0}};
	CollectionEntry yield_entries[1] = {{&mock_yield, 0}};
	CollectionEntry callback_entries[1] = {{&mock_callback, 0}};

	VectorTriple prop_vec{reinterpret_cast<std::uintptr_t>(&prop_entries[0]), reinterpret_cast<std::uintptr_t>(&prop_entries[1]), reinterpret_cast<std::uintptr_t>(&prop_entries[1])};
	VectorTriple event_vec{reinterpret_cast<std::uintptr_t>(&event_entries[0]), reinterpret_cast<std::uintptr_t>(&event_entries[1]), reinterpret_cast<std::uintptr_t>(&event_entries[1])};
	VectorTriple func_vec{reinterpret_cast<std::uintptr_t>(&func_entries[0]), reinterpret_cast<std::uintptr_t>(&func_entries[1]), reinterpret_cast<std::uintptr_t>(&func_entries[1])};
	VectorTriple yield_vec{reinterpret_cast<std::uintptr_t>(&yield_entries[0]), reinterpret_cast<std::uintptr_t>(&yield_entries[1]), reinterpret_cast<std::uintptr_t>(&yield_entries[1])};
	VectorTriple callback_vec{reinterpret_cast<std::uintptr_t>(&callback_entries[0]), reinterpret_cast<std::uintptr_t>(&callback_entries[1]), reinterpret_cast<std::uintptr_t>(&callback_entries[1])};

	std::vector<std::byte> class_buf(0x400, std::byte{0});

	std::array<std::ptrdiff_t, 5> test_offsets = {0x28, 0x88, 0xe8, 0x148, 0x1a8};
	std::memcpy(class_buf.data() + test_offsets[0], &prop_vec, sizeof(prop_vec));
	std::memcpy(class_buf.data() + test_offsets[1], &event_vec, sizeof(event_vec));
	std::memcpy(class_buf.data() + test_offsets[2], &func_vec, sizeof(func_vec));
	std::memcpy(class_buf.data() + test_offsets[3], &yield_vec, sizeof(yield_vec));
	std::memcpy(class_buf.data() + test_offsets[4], &callback_vec, sizeof(callback_vec));

	const auto functionality_offset = 0x220;
	std::uint32_t functionality_flags = 0x8;
	std::memcpy(class_buf.data() + functionality_offset, &functionality_flags, sizeof(functionality_flags));
	ReflectionCapabilities caps_storage(
		mock_get_string_atom,
		test_offsets,
		0x228,
		0x220,
		0x8,
		0x30,
		0x38,
		0x40,
		0x8c,
		0x48,
		0x78,
		0x80,
		0x88,
		0x40,
		0x70,
		0x78);
	const auto* caps = &caps_storage;
	const auto* class_desc =
		reinterpret_cast<const RBX::Reflection::ClassDescriptor*>(class_buf.data());
	if (!caps->find_property(class_desc, "TestProperty"))
	{
		std::cerr << "Test 6a failed: find_property direct lookup\n";
		return 6;
	}

	if (!caps->find_event(class_desc, "TestEvent"))
	{
		std::cerr << "Test 6b failed: find_event direct lookup\n";
		return 6;
	}

	if (!caps->find_function(class_desc, "TestFunction"))
	{
		std::cerr << "Test 6c failed: find_function direct lookup\n";
		return 6;
	}

	if (!caps->find_yield_function(class_desc, "TestYieldFunction"))
	{
		std::cerr << "Test 6d failed: find_yield_function direct lookup\n";
		return 6;
	}

	if (!caps->find_callback(class_desc, "TestCallback"))
	{
		std::cerr << "Test 6e failed: find_callback direct lookup\n";
		return 6;
	}

	if (!caps->is_serializable(class_desc))
	{
		std::cerr << "Test 6f failed: is_serializable check\n";
		return 6;
	}

	// 7. Inherited lookup test across all 5 families
	std::vector<std::byte> child_class_buf(0x400, std::byte{0});
	const auto base_class_offset = 0x228;
	const auto parent_address = reinterpret_cast<std::uintptr_t>(class_buf.data());
	std::memcpy(child_class_buf.data() + base_class_offset, &parent_address, sizeof(parent_address));

	const auto* child_class_desc = reinterpret_cast<const RBX::Reflection::ClassDescriptor*>(child_class_buf.data());
	if (!caps->find_property(child_class_desc, "TestProperty") ||
		!caps->find_event(child_class_desc, "TestEvent") ||
		!caps->find_function(child_class_desc, "TestFunction") ||
		!caps->find_yield_function(child_class_desc, "TestYieldFunction") ||
		!caps->find_callback(child_class_desc, "TestCallback"))
	{
		std::cerr << "Test 7 failed: inherited lookup across all 5 families\n";
		return 7;
	}

	// 8. Base class cycle test (fail closed, no hang)
	const auto child_address = reinterpret_cast<std::uintptr_t>(child_class_buf.data());
	std::memcpy(class_buf.data() + base_class_offset, &child_address, sizeof(child_address));

	if (caps->find_property(child_class_desc, "NonExistent") ||
		caps->find_event(child_class_desc, "NonExistent") ||
		caps->find_function(child_class_desc, "NonExistent") ||
		caps->find_yield_function(child_class_desc, "NonExistent") ||
		caps->find_callback(child_class_desc, "NonExistent"))
	{
		std::cerr << "Test 8 failed: cycle returned non-null member\n";
		return 8;
	}

	if (caps->is_a(child_class_desc, "NonExistent"))
	{
		std::cerr << "Test 8b failed: is_a cycle returned true\n";
		return 8;
	}
	// 8c. Root null base traversal test
	std::uintptr_t zero_base = 0;
	std::memcpy(class_buf.data() + base_class_offset, &zero_base, sizeof(zero_base));
	const auto root_null_res = caps->base_class(class_desc);
	if (!root_null_res.has_value() || root_null_res.value() != nullptr)
	{
		std::cerr << "Test 8c failed: root null base traversal did not return expected nullptr\n";
		return 8;
	}

	// 8d. Malformed non-null base traversal test
	std::vector<std::byte> malformed_base_buf(0x400, std::byte{0});
	const std::uintptr_t bad_base_ptr = 0x12345; // unaligned misaligned address
	std::memcpy(malformed_base_buf.data() + base_class_offset, &bad_base_ptr, sizeof(bad_base_ptr));
	const auto* bad_base_desc = reinterpret_cast<const RBX::Reflection::ClassDescriptor*>(malformed_base_buf.data());
	const auto bad_base_res = caps->base_class(bad_base_desc);
	if (bad_base_res.has_value() || bad_base_res.error().failure != rml::roblox::internals::CompatibilityFailure::invalid_address_range)
	{
		std::cerr << "Test 8d failed: malformed non-null base did not yield typed compatibility failure\n";
		return 8;
	}
	if (caps->is_a(bad_base_desc, "NonExistent"))
	{
		std::cerr << "Test 8e failed: is_a on malformed non-null base returned true\n";
		return 8;
	}
	if (caps->find_property(bad_base_desc, "TestProperty"))
	{
		std::cerr << "Test 8f failed: find_property on malformed non-null base returned non-null member\n";
		return 8;
	}

	// 9. Malformed vector tests (fail closed)
	std::vector<std::byte> malformed_class_buf(0x400, std::byte{0});

	// Unaligned my_first
	VectorTriple unaligned_vec{0x1, 0x100, 0x100};
	std::memcpy(malformed_class_buf.data() + test_offsets[0], &unaligned_vec, sizeof(unaligned_vec));
	const auto* malformed_desc = reinterpret_cast<const RBX::Reflection::ClassDescriptor*>(malformed_class_buf.data());

	if (caps->find_property(malformed_desc, "TestProperty"))
	{
		std::cerr << "Test 9a failed: malformed unaligned vector returned non-null\n";
		return 9;
	}

	// Reversed vector pointers (my_last < my_first)
	VectorTriple reversed_vec{0x200, 0x100, 0x200};
	std::memcpy(malformed_class_buf.data() + test_offsets[0], &reversed_vec, sizeof(reversed_vec));
	if (caps->find_property(malformed_desc, "TestProperty"))
	{
		std::cerr << "Test 9b failed: malformed reversed vector returned non-null\n";
		return 9;
	}

	// 10. Large constructor span (> 256 bytes) regression test.
	std::array<std::byte, 1024> large_code{};
	std::size_t large_size = 0;
	large_code[large_size++] = std::byte{0x48};
	large_code[large_size++] = std::byte{0x8d};
	large_code[large_size++] = std::byte{0x05};
	const auto large_vft_instruction = image_base + large_size - 3;
	append_u32(
		large_code,
		large_size,
		static_cast<std::uint32_t>(
			static_cast<std::int32_t>(vft_target - (large_vft_instruction + 7))));
	large_code[large_size++] = std::byte{0x48};
	large_code[large_size++] = std::byte{0x89};
	large_code[large_size++] = std::byte{0x01};
	for (std::size_t i = 0; i < 300; ++i)
		large_code[large_size++] = std::byte{0x90};
	large_code[large_size++] = std::byte{0x4d};
	large_code[large_size++] = std::byte{0x31};
	large_code[large_size++] = std::byte{0xc9};
	for (const auto offset : layout_0732_containers)
	{
		large_code[large_size++] = std::byte{0x4c};
		large_code[large_size++] = std::byte{0x8d};
		large_code[large_size++] = std::byte{0x81};
		append_u32(large_code, large_size, offset);
		large_code[large_size++] = std::byte{0x4d};
		large_code[large_size++] = std::byte{0x89};
		large_code[large_size++] = std::byte{0x08};
		large_code[large_size++] = std::byte{0x4d};
		large_code[large_size++] = std::byte{0x89};
		large_code[large_size++] = std::byte{0x48};
		large_code[large_size++] = std::byte{0x08};
		large_code[large_size++] = std::byte{0x4d};
		large_code[large_size++] = std::byte{0x89};
		large_code[large_size++] = std::byte{0x48};
		large_code[large_size++] = std::byte{0x10};
	}
	large_code[large_size++] = std::byte{0x4c};
	large_code[large_size++] = std::byte{0x8b};
	large_code[large_size++] = std::byte{0x82};
	append_u32(large_code, large_size, 0x1a8);
	large_code[large_size++] = std::byte{0x4c};
	large_code[large_size++] = std::byte{0x89};
	large_code[large_size++] = std::byte{0x81};
	append_u32(large_code, large_size, 0x1a8);
	large_code[large_size++] = std::byte{0xf7};
	large_code[large_size++] = std::byte{0x81};
	append_u32(large_code, large_size, 0x1bc);
	append_u32(large_code, large_size, 0x08);
	large_code[large_size++] = std::byte{0xc3};

	const auto large_layout = resolve(large_code, large_size);
	if (!large_layout ||
		large_layout->descriptor_container_offsets !=
			std::array<std::ptrdiff_t, 5>{0x28, 0x70, 0xb8, 0x100, 0x148} ||
		large_layout->base_class_offset != 0x1a8 ||
		large_layout->functionality_offset != 0x1bc)
	{
		std::cerr << "Test 10 failed: large constructor >256 bytes regression test\n";
		return 10;
	}

	// 11. Adversarial noise test with unrelated LEAs, stores, and bitmasks
	std::array<std::byte, 1024> adv_code{};
	std::size_t adv_size = 0;

	// LEA RAX, [RIP + disp] -> vft_target
	adv_code[adv_size++] = std::byte{0x48};
	adv_code[adv_size++] = std::byte{0x8d};
	adv_code[adv_size++] = std::byte{0x05};
	const auto inst_addr_a = image_base + adv_size - 3;
	const auto target_disp_a = static_cast<std::int32_t>(vft_target - (inst_addr_a + 7));
	append_u32(adv_code, adv_size, static_cast<std::uint32_t>(target_disp_a));

	// MOV [RCX], RAX -> sets this_reg = RCX
	adv_code[adv_size++] = std::byte{0x48};
	adv_code[adv_size++] = std::byte{0x89};
	adv_code[adv_size++] = std::byte{0x01};

	// Unrelated noise LEAs without vector triple stores
	adv_code[adv_size++] = std::byte{0x4c};
	adv_code[adv_size++] = std::byte{0x8d};
	adv_code[adv_size++] = std::byte{0x81};
	append_u32(adv_code, adv_size, 0x50); // LEA R8, [RCX + 0x50] (no stores follow)

	// XOR R8, R8 (zero reg)
	adv_code[adv_size++] = std::byte{0x4d};
	adv_code[adv_size++] = std::byte{0x31};
	adv_code[adv_size++] = std::byte{0xc0};

	// Real 5 container vector blocks (LEA R9, [RCX + offset] followed by zero stores to [R9+0], [R9+8], [R9+0x10])
	for (const auto offset : layout_0731_containers)
	{
		// LEA R9, [RCX + offset]
		adv_code[adv_size++] = std::byte{0x4c};
		adv_code[adv_size++] = std::byte{0x8d};
		adv_code[adv_size++] = std::byte{0x89};
		append_u32(adv_code, adv_size, offset);

		// MOV [R9 + 0], R8
		adv_code[adv_size++] = std::byte{0x4d};
		adv_code[adv_size++] = std::byte{0x89};
		adv_code[adv_size++] = std::byte{0x01};

		// MOV [R9 + 8], R8
		adv_code[adv_size++] = std::byte{0x4d};
		adv_code[adv_size++] = std::byte{0x89};
		adv_code[adv_size++] = std::byte{0x41};
		adv_code[adv_size++] = std::byte{0x08};

		// MOV [R9 + 0x10], R8
		adv_code[adv_size++] = std::byte{0x4d};
		adv_code[adv_size++] = std::byte{0x89};
		adv_code[adv_size++] = std::byte{0x41};
		adv_code[adv_size++] = std::byte{0x10};
	}

	// Unrelated bitmask 0x8 operation on RAX (not stored to RCX)
	adv_code[adv_size++] = std::byte{0xa8};
	adv_code[adv_size++] = std::byte{0x08}; // TEST AL, 0x8

	// Base class load & store: MOV R8, [RDX + 0x228] -> MOV [RCX + 0x228], R8
	adv_code[adv_size++] = std::byte{0x4c};
	adv_code[adv_size++] = std::byte{0x8b};
	adv_code[adv_size++] = std::byte{0x82};
	append_u32(adv_code, adv_size, 0x228);

	adv_code[adv_size++] = std::byte{0x4c};
	adv_code[adv_size++] = std::byte{0x89};
	adv_code[adv_size++] = std::byte{0x81};
	append_u32(adv_code, adv_size, 0x228);

	// Register-based bitmask 0x8 update for functionality 0x220: TEST EAX, 0x8 -> MOV [RCX + 0x220], EAX
	adv_code[adv_size++] = std::byte{0xa8};
	adv_code[adv_size++] = std::byte{0x08};

	adv_code[adv_size++] = std::byte{0x89};
	adv_code[adv_size++] = std::byte{0x81};
	append_u32(adv_code, adv_size, 0x220);

	// RET
	adv_code[adv_size++] = std::byte{0xc3};

	auto adv_layout = resolve(adv_code, adv_size);
	if (!adv_layout ||
		adv_layout->descriptor_container_offsets != std::array<std::ptrdiff_t, 5>{0x28, 0x88, 0xe8, 0x148, 0x1a8} ||
		adv_layout->base_class_offset != 0x228 ||
		adv_layout->functionality_offset != 0x220)
	{
		std::cerr << "Test 11 failed: adversarial noise test\n";
		return 11;
	}

	// 12. PropertyDescriptorSpanView empty range-iteration regression test ({nullptr, 0})
	rml::roblox::internals::PropertyDescriptorSpanView empty_span{nullptr, 0};
	std::size_t empty_iterations = 0;
	for (const auto& entry : empty_span)
	{
		(void)entry;
		++empty_iterations;
	}
	if (empty_span.begin() != nullptr || empty_span.end() != nullptr || empty_iterations != 0)
	{
		std::cerr << "Test 12 failed: empty PropertyDescriptorSpanView iteration failed\n";
		return 12;
	}

	rml::roblox::internals::PropertyDescriptorCollectionEntry dummy_entry{nullptr, 0};
	rml::roblox::internals::PropertyDescriptorSpanView nonempty_span{&dummy_entry, 1};
	std::size_t nonempty_iterations = 0;
	for (const auto& entry : nonempty_span)
	{
		(void)entry;
		++nonempty_iterations;
	}
	if (nonempty_span.begin() != &dummy_entry || nonempty_span.end() != &dummy_entry + 1 || nonempty_iterations != 1)
	{
		std::cerr << "Test 12b failed: non-empty PropertyDescriptorSpanView iteration failed\n";
		return 12;
	}

	// 13. Zero-after-store false-positive regression test: XOR R9, R9 appears AFTER container stores
	std::array<std::byte, 1024> zero_after_code{};
	std::size_t zero_after_size = 0;

	// LEA RAX, [RIP + disp] -> vft_target
	zero_after_code[zero_after_size++] = std::byte{0x48};
	zero_after_code[zero_after_size++] = std::byte{0x8d};
	zero_after_code[zero_after_size++] = std::byte{0x05};
	const auto inst_addr_za = image_base + zero_after_size - 3;
	const auto target_disp_za = static_cast<std::int32_t>(vft_target - (inst_addr_za + 7));
	append_u32(zero_after_code, zero_after_size, static_cast<std::uint32_t>(target_disp_za));

	// MOV [RCX], RAX -> sets this_reg = RCX
	zero_after_code[zero_after_size++] = std::byte{0x48};
	zero_after_code[zero_after_size++] = std::byte{0x89};
	zero_after_code[zero_after_size++] = std::byte{0x01};

	// Container stores BEFORE zeroing R9
	for (const auto offset : layout_0731_containers)
	{
		// LEA R8, [RCX + offset]
		zero_after_code[zero_after_size++] = std::byte{0x4c};
		zero_after_code[zero_after_size++] = std::byte{0x8d};
		zero_after_code[zero_after_size++] = std::byte{0x81};
		append_u32(zero_after_code, zero_after_size, offset);

		// MOV [R8 + 0], R9
		zero_after_code[zero_after_size++] = std::byte{0x4d};
		zero_after_code[zero_after_size++] = std::byte{0x89};
		zero_after_code[zero_after_size++] = std::byte{0x08};

		// MOV [R8 + 8], R9
		zero_after_code[zero_after_size++] = std::byte{0x4d};
		zero_after_code[zero_after_size++] = std::byte{0x89};
		zero_after_code[zero_after_size++] = std::byte{0x48};
		zero_after_code[zero_after_size++] = std::byte{0x08};

		// MOV [R8 + 0x10], R9
		zero_after_code[zero_after_size++] = std::byte{0x4d};
		zero_after_code[zero_after_size++] = std::byte{0x89};
		zero_after_code[zero_after_size++] = std::byte{0x48};
		zero_after_code[zero_after_size++] = std::byte{0x10};
	}

	// XOR R9, R9 (zero reg AFTER stores)
	zero_after_code[zero_after_size++] = std::byte{0x4d};
	zero_after_code[zero_after_size++] = std::byte{0x31};
	zero_after_code[zero_after_size++] = std::byte{0xc9};

	// Base class load & store: MOV R8, [RDX + 0x228] -> MOV [RCX + 0x228], R8
	zero_after_code[zero_after_size++] = std::byte{0x4c};
	zero_after_code[zero_after_size++] = std::byte{0x8b};
	zero_after_code[zero_after_size++] = std::byte{0x82};
	append_u32(zero_after_code, zero_after_size, 0x228);

	zero_after_code[zero_after_size++] = std::byte{0x4c};
	zero_after_code[zero_after_size++] = std::byte{0x89};
	zero_after_code[zero_after_size++] = std::byte{0x81};
	append_u32(zero_after_code, zero_after_size, 0x228);

	// Bitmask update for functionality 0x220: TEST dword ptr [RCX + 0x220], 0x8
	zero_after_code[zero_after_size++] = std::byte{0xf7};
	zero_after_code[zero_after_size++] = std::byte{0x81};
	append_u32(zero_after_code, zero_after_size, 0x220);
	append_u32(zero_after_code, zero_after_size, 0x08);

	// RET
	zero_after_code[zero_after_size++] = std::byte{0xc3};

	auto zero_after_res = resolve(zero_after_code, zero_after_size);
	if (zero_after_res)
	{
		std::cerr << "Test 13 failed: zero-after-store false-positive form was incorrectly validated\n";
		return 13;
	}

	// 14. Clobbered-before-store false-positive regression test: XOR R9, R9 followed by MOV R9, RDX before stores
	std::array<std::byte, 1024> clobbered_code{};
	std::size_t clobbered_size = 0;

	// LEA RAX, [RIP + disp] -> vft_target
	clobbered_code[clobbered_size++] = std::byte{0x48};
	clobbered_code[clobbered_size++] = std::byte{0x8d};
	clobbered_code[clobbered_size++] = std::byte{0x05};
	const auto inst_addr_cl = image_base + clobbered_size - 3;
	const auto target_disp_cl = static_cast<std::int32_t>(vft_target - (inst_addr_cl + 7));
	append_u32(clobbered_code, clobbered_size, static_cast<std::uint32_t>(target_disp_cl));

	// MOV [RCX], RAX -> sets this_reg = RCX
	clobbered_code[clobbered_size++] = std::byte{0x48};
	clobbered_code[clobbered_size++] = std::byte{0x89};
	clobbered_code[clobbered_size++] = std::byte{0x01};

	// XOR R9, R9 (zero reg)
	clobbered_code[clobbered_size++] = std::byte{0x4d};
	clobbered_code[clobbered_size++] = std::byte{0x31};
	clobbered_code[clobbered_size++] = std::byte{0xc9};

	// Clobber R9: MOV R9, RDX
	clobbered_code[clobbered_size++] = std::byte{0x49};
	clobbered_code[clobbered_size++] = std::byte{0x89};
	clobbered_code[clobbered_size++] = std::byte{0xd1};

	// Container stores using clobbered R9
	for (const auto offset : layout_0731_containers)
	{
		// LEA R8, [RCX + offset]
		clobbered_code[clobbered_size++] = std::byte{0x4c};
		clobbered_code[clobbered_size++] = std::byte{0x8d};
		clobbered_code[clobbered_size++] = std::byte{0x81};
		append_u32(clobbered_code, clobbered_size, offset);

		// MOV [R8 + 0], R9
		clobbered_code[clobbered_size++] = std::byte{0x4d};
		clobbered_code[clobbered_size++] = std::byte{0x89};
		clobbered_code[clobbered_size++] = std::byte{0x08};

		// MOV [R8 + 8], R9
		clobbered_code[clobbered_size++] = std::byte{0x4d};
		clobbered_code[clobbered_size++] = std::byte{0x89};
		clobbered_code[clobbered_size++] = std::byte{0x48};
		clobbered_code[clobbered_size++] = std::byte{0x08};

		// MOV [R8 + 0x10], R9
		clobbered_code[clobbered_size++] = std::byte{0x4d};
		clobbered_code[clobbered_size++] = std::byte{0x89};
		clobbered_code[clobbered_size++] = std::byte{0x48};
		clobbered_code[clobbered_size++] = std::byte{0x10};
	}

	// Base class load & store: MOV R8, [RDX + 0x228] -> MOV [RCX + 0x228], R8
	clobbered_code[clobbered_size++] = std::byte{0x4c};
	clobbered_code[clobbered_size++] = std::byte{0x8b};
	clobbered_code[clobbered_size++] = std::byte{0x82};
	append_u32(clobbered_code, clobbered_size, 0x228);

	clobbered_code[clobbered_size++] = std::byte{0x4c};
	clobbered_code[clobbered_size++] = std::byte{0x89};
	clobbered_code[clobbered_size++] = std::byte{0x81};
	append_u32(clobbered_code, clobbered_size, 0x228);

	// Bitmask update for functionality 0x220: TEST dword ptr [RCX + 0x220], 0x8
	clobbered_code[clobbered_size++] = std::byte{0xf7};
	clobbered_code[clobbered_size++] = std::byte{0x81};
	append_u32(clobbered_code, clobbered_size, 0x220);
	append_u32(clobbered_code, clobbered_size, 0x08);

	// RET
	clobbered_code[clobbered_size++] = std::byte{0xc3};

	auto clobbered_res = resolve(clobbered_code, clobbered_size);
	if (clobbered_res)
	{
		std::cerr << "Test 14 failed: clobbered-before-store false-positive form was incorrectly validated\n";
		return 14;
	}

	// 15. Immediate-zero MOV reg, 0 provenance resolution test
	std::array<std::byte, 1024> mov_zero_code{};
	std::size_t mov_zero_size = 0;

	// LEA RAX, [RIP + disp] -> vft_target
	mov_zero_code[mov_zero_size++] = std::byte{0x48};
	mov_zero_code[mov_zero_size++] = std::byte{0x8d};
	mov_zero_code[mov_zero_size++] = std::byte{0x05};
	const auto inst_addr_mz = image_base + mov_zero_size - 3;
	const auto target_disp_mz = static_cast<std::int32_t>(vft_target - (inst_addr_mz + 7));
	append_u32(mov_zero_code, mov_zero_size, static_cast<std::uint32_t>(target_disp_mz));

	// MOV [RCX], RAX -> sets this_reg = RCX
	mov_zero_code[mov_zero_size++] = std::byte{0x48};
	mov_zero_code[mov_zero_size++] = std::byte{0x89};
	mov_zero_code[mov_zero_size++] = std::byte{0x01};

	// MOV R9, 0 -> 49 c7 c1 00 00 00 00
	mov_zero_code[mov_zero_size++] = std::byte{0x49};
	mov_zero_code[mov_zero_size++] = std::byte{0xc7};
	mov_zero_code[mov_zero_size++] = std::byte{0xc1};
	append_u32(mov_zero_code, mov_zero_size, 0);

	// Container stores using MOV-zero R9
	for (const auto offset : layout_0731_containers)
	{
		// LEA R8, [RCX + offset]
		mov_zero_code[mov_zero_size++] = std::byte{0x4c};
		mov_zero_code[mov_zero_size++] = std::byte{0x8d};
		mov_zero_code[mov_zero_size++] = std::byte{0x81};
		append_u32(mov_zero_code, mov_zero_size, offset);

		// MOV [R8 + 0], R9
		mov_zero_code[mov_zero_size++] = std::byte{0x4d};
		mov_zero_code[mov_zero_size++] = std::byte{0x89};
		mov_zero_code[mov_zero_size++] = std::byte{0x08};

		// MOV [R8 + 8], R9
		mov_zero_code[mov_zero_size++] = std::byte{0x4d};
		mov_zero_code[mov_zero_size++] = std::byte{0x89};
		mov_zero_code[mov_zero_size++] = std::byte{0x48};
		mov_zero_code[mov_zero_size++] = std::byte{0x08};

		// MOV [R8 + 0x10], R9
		mov_zero_code[mov_zero_size++] = std::byte{0x4d};
		mov_zero_code[mov_zero_size++] = std::byte{0x89};
		mov_zero_code[mov_zero_size++] = std::byte{0x48};
		mov_zero_code[mov_zero_size++] = std::byte{0x10};
	}

	// Base class load & store: MOV R8, [RDX + 0x228] -> MOV [RCX + 0x228], R8
	mov_zero_code[mov_zero_size++] = std::byte{0x4c};
	mov_zero_code[mov_zero_size++] = std::byte{0x8b};
	mov_zero_code[mov_zero_size++] = std::byte{0x82};
	append_u32(mov_zero_code, mov_zero_size, 0x228);

	mov_zero_code[mov_zero_size++] = std::byte{0x4c};
	mov_zero_code[mov_zero_size++] = std::byte{0x89};
	mov_zero_code[mov_zero_size++] = std::byte{0x81};
	append_u32(mov_zero_code, mov_zero_size, 0x228);

	// Bitmask update for functionality 0x220: TEST dword ptr [RCX + 0x220], 0x8
	mov_zero_code[mov_zero_size++] = std::byte{0xf7};
	mov_zero_code[mov_zero_size++] = std::byte{0x81};
	append_u32(mov_zero_code, mov_zero_size, 0x220);
	append_u32(mov_zero_code, mov_zero_size, 0x08);

	// RET
	mov_zero_code[mov_zero_size++] = std::byte{0xc3};

	auto mov_zero_res = resolve(mov_zero_code, mov_zero_size);
	if (!mov_zero_res ||
		!std::equal(mov_zero_res->descriptor_container_offsets.begin(), mov_zero_res->descriptor_container_offsets.end(), layout_0731_containers.begin(), layout_0731_containers.end()) ||
		mov_zero_res->base_class_offset != 0x228 ||
		mov_zero_res->functionality_offset != 0x220)
	{
		std::cerr << "Test 15 failed: MOV reg, 0 immediate zero resolution failed\n";
		return 15;
	}

	// 16. Instance, Signal, and Job layout resolvers with encoded PE instruction streams.
	{
		#pragma pack(push, 1)
		struct TestRuntimeFunction
		{
			std::uint32_t begin_address;
			std::uint32_t end_address;
			std::uint32_t unwind_info_address;
		};
		#pragma pack(pop)

		const std::uintptr_t base_addr = 0x140000000;
		const std::uintptr_t code_addr = base_addr + 0x1000;
		const std::uintptr_t vft_addr = base_addr + 0x5000;
		const std::uintptr_t wrong_vft_addr = base_addr + 0x6000;
		const std::span vfts_span{&vft_addr, 1};
		const std::span wrong_vfts_span{&wrong_vft_addr, 1};

		// --- Instance 0.732 Positive Fixture ---
		std::vector<std::uint8_t> inst_code;
		// Function 1: Constructor-shaped initialization from current MSVC Studio output.
		// MOV RBX, RCX; XOR ESI, ESI
		inst_code.insert(inst_code.end(), {0x48, 0x8B, 0xD9, 0x33, 0xF6});
		// parent shared ownership pair at 0x60
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0x73, 0x60});
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0x73, 0x68});
		const auto xref_off = inst_code.size();
		// LEA RAX, [RIP + disp]; MOV [RBX], RAX
		const auto target_disp = static_cast<std::int32_t>(vft_addr - (code_addr + xref_off + 7));
		inst_code.insert(inst_code.end(), {0x48, 0x8D, 0x05});
		const auto* disp_bytes = reinterpret_cast<const std::uint8_t*>(&target_disp);
		inst_code.insert(inst_code.end(), disp_bytes, disp_bytes + 4);
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0x03});
		// children three-word ownership object at 0x70
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0x73, 0x70});
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0x73, 0x78});
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0xB3, 0x80, 0x00, 0x00, 0x00});
		// Atom-producing call followed by the name field store.
		inst_code.insert(inst_code.end(), {0xE8, 0x00, 0x00, 0x00, 0x00});
		inst_code.insert(inst_code.end(), {0x48, 0x89, 0x83, 0x98, 0x00, 0x00, 0x00});
		inst_code.push_back(0xC3);
		const auto inst_fn1_end = inst_code.size();

		// Function 2: Standalone trivial name getter: MOV RAX, [RCX + 0x98] ; RET
		inst_code.insert(inst_code.end(), {0x48, 0x8B, 0x81, 0x98, 0x00, 0x00, 0x00});
		inst_code.push_back(0xC3);

		const std::array<TestRuntimeFunction, 2> inst_pdata{
			TestRuntimeFunction{
				.begin_address = 0x1000,
				.end_address = static_cast<std::uint32_t>(0x1000 + inst_fn1_end),
				.unwind_info_address = 1,
			},
			TestRuntimeFunction{
				.begin_address = static_cast<std::uint32_t>(0x1000 + inst_fn1_end),
				.end_address = static_cast<std::uint32_t>(0x1000 + inst_code.size()),
				.unwind_info_address = 1,
			},
		};

		const std::uintptr_t name_getter_addr = code_addr + inst_fn1_end;
		const std::span inst_entries_span{&name_getter_addr, 1};

		const auto inst_res = rml::roblox::internals::resolve_instance_layout(
			std::as_bytes(std::span{inst_code}),
			code_addr,
			std::as_bytes(std::span{inst_pdata}),
			base_addr,
			vfts_span,
			inst_entries_span);

		if (!inst_res || inst_res->parent_offset != 0x60 || inst_res->children_offset != 0x70 || inst_res->name_offset != 0x98)
		{
			std::cerr << "Test 16 failed: Instance 0.732 dynamic layout resolution\n";
			return 16;
		}
		// Instance Negatives: wrong VFT & missing name
		const auto inst_wrong_vft = rml::roblox::internals::resolve_instance_layout(
			std::as_bytes(std::span{inst_code}), code_addr, std::as_bytes(std::span{inst_pdata}),
			base_addr, wrong_vfts_span, inst_entries_span);
		if (inst_wrong_vft)
		{
			std::cerr << "Test 16 failed: Instance wrong VFT did not fail closed\n";
			return 16;
		}

		// --- Job Positive Fixture ---
		const std::uintptr_t dm_job_vft_addr = base_addr + 0x5000;
		const std::uintptr_t waiting_job_vft_addr = base_addr + 0x5100;
		const std::span dm_job_vfts_span{&dm_job_vft_addr, 1};
		const std::span waiting_job_vfts_span{&waiting_job_vft_addr, 1};

		std::vector<std::uint8_t> job_code;
		// Function 1: DataModelJob constructor
		const auto j_xref_off = job_code.size();
		const auto j_target_disp = static_cast<std::int32_t>(dm_job_vft_addr - (code_addr + j_xref_off + 7));
		job_code.insert(job_code.end(), {0x4C, 0x8D, 0x05});
		const auto* j_disp_bytes = reinterpret_cast<const std::uint8_t*>(&j_target_disp);
		job_code.insert(job_code.end(), j_disp_bytes, j_disp_bytes + 4);
		// MOV [RCX], R8
		job_code.insert(job_code.end(), {0x4C, 0x89, 0x01});
		// MOV [RCX + 0x38], RDX  (job_data_model = 0x38)
		job_code.insert(job_code.end(), {0x48, 0x89, 0x51, 0x38});
		// RET
		job_code.push_back(0xC3);
		const auto fn1_end = job_code.size();

		// Function 2: WaitingHybridScriptsJob constructor
		const auto wj_xref_off = job_code.size();
		const auto wj_target_disp = static_cast<std::int32_t>(waiting_job_vft_addr - (code_addr + wj_xref_off + 7));
		job_code.insert(job_code.end(), {0x4C, 0x8D, 0x05});
		const auto* wj_disp_bytes = reinterpret_cast<const std::uint8_t*>(&wj_target_disp);
		job_code.insert(job_code.end(), wj_disp_bytes, wj_disp_bytes + 4);
		// MOV [RCX], R8
		job_code.insert(job_code.end(), {0x4C, 0x89, 0x01});
		// MOV [RCX + 0x1F8], RDX  (script_context = 0x1F8)
		job_code.insert(job_code.end(), {0x48, 0x89, 0x91, 0xF8, 0x01, 0x00, 0x00});
		// RET
		job_code.push_back(0xC3);

		const std::array<TestRuntimeFunction, 2> job_pdata{
			TestRuntimeFunction{
				.begin_address = 0x1000,
				.end_address = static_cast<std::uint32_t>(0x1000 + fn1_end),
				.unwind_info_address = 1,
			},
			TestRuntimeFunction{
				.begin_address = static_cast<std::uint32_t>(0x1000 + fn1_end),
				.end_address = static_cast<std::uint32_t>(0x1000 + job_code.size()),
				.unwind_info_address = 1,
			},
		};
		const auto job_res = rml::roblox::internals::resolve_job_layout(
			std::as_bytes(std::span{job_code}),
			code_addr,
			std::as_bytes(std::span{job_pdata}),
			base_addr,
			waiting_job_vfts_span);

		if (!job_res || job_res->waiting_scripts_job_script_context_offset != 0x1F8)
		{
			std::cerr << "Test 16 failed: Job dynamic layout resolution\n";
			return 16;
		}

		// Job Negatives: register clobber before store
		std::vector<std::uint8_t> job_clobber_code;
		const auto jc_xref_off = job_clobber_code.size();
		const auto jc_target_disp = static_cast<std::int32_t>(waiting_job_vft_addr - (code_addr + jc_xref_off + 7));
		job_clobber_code.insert(job_clobber_code.end(), {0x4C, 0x8D, 0x05});
		const auto* jc_disp_bytes = reinterpret_cast<const std::uint8_t*>(&jc_target_disp);
		job_clobber_code.insert(job_clobber_code.end(), jc_disp_bytes, jc_disp_bytes + 4);
		// MOV [RCX], R8
		job_clobber_code.insert(job_clobber_code.end(), {0x4C, 0x89, 0x01});
		// XOR RDX, RDX  (clobber RDX)
		job_clobber_code.insert(job_clobber_code.end(), {0x48, 0x31, 0xD2});
		// MOV [RCX + 0x38], RDX
		job_clobber_code.insert(job_clobber_code.end(), {0x48, 0x89, 0x51, 0x38});
		// RET
		job_clobber_code.push_back(0xC3);

		const TestRuntimeFunction job_clobber_pdata{
			.begin_address = 0x1000,
			.end_address = static_cast<std::uint32_t>(0x1000 + job_clobber_code.size()),
			.unwind_info_address = 1,
		};
		const auto job_clobber_res = rml::roblox::internals::resolve_job_layout(
			std::as_bytes(std::span{job_clobber_code}), code_addr, std::as_bytes(std::span{&job_clobber_pdata, 1}), base_addr, waiting_job_vfts_span);
		if (job_clobber_res)
		{
			std::cerr << "Test 16 failed: Job register clobber did not fail closed\n";
			return 16;
		}

		// Job Negative: unrelated third constructor argument
		std::vector<std::uint8_t> job_arg3_code;
		const auto ja_xref_off = job_arg3_code.size();
		const auto ja_target_disp = static_cast<std::int32_t>(waiting_job_vft_addr - (code_addr + ja_xref_off + 7));
		job_arg3_code.insert(job_arg3_code.end(), {0x4C, 0x8D, 0x05});
		const auto* ja_disp_bytes = reinterpret_cast<const std::uint8_t*>(&ja_target_disp);
		job_arg3_code.insert(job_arg3_code.end(), ja_disp_bytes, ja_disp_bytes + 4);
		// MOV [RCX], R8
		job_arg3_code.insert(job_arg3_code.end(), {0x4C, 0x89, 0x01});
		// MOV [RCX + 0x1B0], R9
		job_arg3_code.insert(job_arg3_code.end(), {0x4C, 0x89, 0x89, 0xB0, 0x01, 0x00, 0x00});
		job_arg3_code.push_back(0xC3);

		const TestRuntimeFunction job_arg3_pdata{
			.begin_address = 0x1000,
			.end_address = static_cast<std::uint32_t>(0x1000 + job_arg3_code.size()),
			.unwind_info_address = 1,
		};
		const auto job_arg3_res = rml::roblox::internals::resolve_job_layout(
			std::as_bytes(std::span{job_arg3_code}), code_addr,
			std::as_bytes(std::span{&job_arg3_pdata, 1}), base_addr, waiting_job_vfts_span);
		if (job_arg3_res)
		{
			std::cerr << "Test 16 failed: Job unrelated argument did not fail closed\n";
			return 16;
		}

		// --- Signal Positive Fixture ---
		// Construct genuine multi-function positive byte fixture satisfying disconnect/unlink/insert/connect semantics.
		std::vector<std::uint8_t> sig_pos_code;

		auto append_bytes = [&](std::initializer_list<std::uint8_t> bytes) {
			sig_pos_code.insert(sig_pos_code.end(), bytes);
		};
		auto append_rel32_call = [&](std::size_t target_offset) {
			sig_pos_code.push_back(0xE8);
			const auto call_site = sig_pos_code.size();
			const auto disp = static_cast<std::int32_t>(
				target_offset - (call_site + 4));
			const auto* disp_bytes = reinterpret_cast<const std::uint8_t*>(&disp);
			sig_pos_code.insert(sig_pos_code.end(), disp_bytes, disp_bytes + 4);
		};

		// Function 1: Disconnect function
		const std::size_t off_disconnect = sig_pos_code.size();
		// MOV RDX, RCX
		append_bytes({0x48, 0x89, 0xCA});
		// MOV RCX, [RCX + 0x20] (source = 32)
		append_bytes({0x48, 0x8B, 0x49, 0x20});
		// Placeholder CALL to unlink_helper (patched below)
		const std::size_t disconnect_call_pos = sig_pos_code.size();
		append_bytes({0xE8, 0x00, 0x00, 0x00, 0x00});
		// RET
		append_bytes({0xC3});

		// Function 2: Unlink helper function
		const std::size_t off_unlink_helper = sig_pos_code.size();
		{
			const auto disp = static_cast<std::int32_t>(
				off_unlink_helper - (disconnect_call_pos + 5));
			const auto* disp_bytes = reinterpret_cast<const std::uint8_t*>(&disp);
			std::copy_n(disp_bytes, 4, sig_pos_code.begin() + disconnect_call_pos + 1);
		}
		// MOV RAX, [RCX + 0x08] (head = 8)
		append_bytes({0x48, 0x8B, 0x41, 0x08});
		// MOV [RCX + 0x08], RAX
		append_bytes({0x48, 0x89, 0x41, 0x08});
		// XOR RAX, RAX
		append_bytes({0x48, 0x31, 0xC0});
		// MOV [RDX + 0x10], RAX (next = 16)
		append_bytes({0x48, 0x89, 0x42, 0x10});
		// MOV RAX, [RCX]
		append_bytes({0x48, 0x8B, 0x01});
		// MOV RAX, [RAX + 0x28] (destroy = 40)
		append_bytes({0x48, 0x8B, 0x40, 0x28});
		// CALL RAX
		append_bytes({0xFF, 0xD0});
		// RET
		append_bytes({0xC3});

		// Function 3: Insert helper function (contains pattern_scan_signal_slot_insert signature)
		const std::size_t off_insert_helper = sig_pos_code.size();
		// Signature bytes: 0x40, 0x53, 0x56, 0x57, 0x48, 0x83, 0xEC, 0x30, 0x48, 0x8B, 0xF2, 0x48, 0x8B, 0xF9, 0x33, 0xDB, 0x89, 0x5C, 0x24, 0x20
		append_bytes({
			0x40, 0x53, 0x56, 0x57, 0x48, 0x83, 0xEC, 0x30,
			0x48, 0x8B, 0xF2, 0x48, 0x8B, 0xF9, 0x33, 0xDB,
			0x89, 0x5C, 0x24, 0x20
		});
		// MOV RAX, [RDI + 0x08] (head = 8)
		append_bytes({0x48, 0x8B, 0x47, 0x08});
		// MOV [RDI + 0x08], RAX
		append_bytes({0x48, 0x89, 0x47, 0x08});
		// LOCK INC dword ptr [RSI] (strong = 0)
		append_bytes({0xF0, 0xFF, 0x06});
		// LEA RAX, [RSI + 0x10] (next = 16)
		append_bytes({0x48, 0x8D, 0x46, 0x10});
		// MOV RCX, RAX
		append_bytes({0x48, 0x89, 0xC1});
		// CALL disconnect_fn
		append_rel32_call(off_disconnect);
		// Epilogue
		append_bytes({0x48, 0x83, 0xC4, 0x30, 0x5F, 0x5E, 0x5B, 0xC3});

		// Function 4: Connect function
		const std::size_t off_connect_fn = sig_pos_code.size();
		// MOV ECX, 0x40 (allocation size = 64)
		append_bytes({0xB9, 0x40, 0x00, 0x00, 0x00});
		// CALL allocator (dummy target: disconnect_fn)
		append_rel32_call(off_disconnect);
		// MOV RBX, RAX (preserve AllocatedSlot role in non-volatile RBX across calls)
		append_bytes({0x48, 0x89, 0xC3});
		// MOV [RBX + 0x20], RDX (source = 32)
		append_bytes({0x48, 0x89, 0x53, 0x20});
		// MOV dword ptr [RBX + 0x00], 0 (strong = 0)
		append_bytes({0xC7, 0x43, 0x00, 0x00, 0x00, 0x00, 0x00});
		// MOV qword ptr [RBX + 0x10], 0 (next = 16)
		append_bytes({0x48, 0xC7, 0x43, 0x10, 0x00, 0x00, 0x00, 0x00});
		// MOV dword ptr [RBX + 0x04], 1 (weak = 4)
		append_bytes({0xC7, 0x43, 0x04, 0x01, 0x00, 0x00, 0x00});
		// MOV R8, [R8 + 0x00] (WrapperPtr setup)
		append_bytes({0x4D, 0x8B, 0x00});
		// MOV R9, [R8 + 0x08] (WrapperRep setup)
		append_bytes({0x4D, 0x8B, 0x48, 0x08});
		// MOV [RBX + 0x30], R8 (wrapper = 48)
		append_bytes({0x4C, 0x89, 0x43, 0x30});
		// MOV [RBX + 0x38], R9 (wrapper_rep = 56)
		append_bytes({0x4C, 0x89, 0x4B, 0x38});
		// MOV RDX, RBX
		append_bytes({0x48, 0x89, 0xDA});
		// CALL insert_helper_fn
		append_rel32_call(off_insert_helper);
		// INC dword ptr [RBX + 0x04] (weak increment)
		append_bytes({0xFF, 0x43, 0x04});
		// RET
		append_bytes({0xC3});

		const std::size_t end_sig_pos_code = sig_pos_code.size();

		const std::array<TestRuntimeFunction, 4> sig_pos_pdata{{
			{
				.begin_address = static_cast<std::uint32_t>(0x1000 + off_disconnect),
				.end_address = static_cast<std::uint32_t>(0x1000 + off_unlink_helper),
				.unwind_info_address = 1,
			},
			{
				.begin_address = static_cast<std::uint32_t>(0x1000 + off_unlink_helper),
				.end_address = static_cast<std::uint32_t>(0x1000 + off_insert_helper),
				.unwind_info_address = 1,
			},
			{
				.begin_address = static_cast<std::uint32_t>(0x1000 + off_insert_helper),
				.end_address = static_cast<std::uint32_t>(0x1000 + off_connect_fn),
				.unwind_info_address = 1,
			},
			{
				.begin_address = static_cast<std::uint32_t>(0x1000 + off_connect_fn),
				.end_address = static_cast<std::uint32_t>(0x1000 + end_sig_pos_code),
				.unwind_info_address = 1,
			},
		}};

		const auto sig_pos_res = rml::roblox::internals::resolve_signal_layout(
			std::as_bytes(std::span{sig_pos_code}),
			code_addr,
			std::as_bytes(std::span{sig_pos_pdata}),
			base_addr,
			code_addr + off_disconnect,
			code_addr + off_disconnect);

		if (!sig_pos_res)
		{
			std::cerr << "Test 16 failed: Signal resolver error "
				<< static_cast<int>(sig_pos_res.error().failure)
				<< ", matched=" << sig_pos_res.error().matched_calls
				<< ", decoded=" << sig_pos_res.error().decoded_candidates << '\n';
			return 16;
		}
		if (sig_pos_res->signal_head_offset != 8 ||
			sig_pos_res->slot_strong_offset != 0 || sig_pos_res->slot_weak_offset != 4 ||
			sig_pos_res->slot_next_offset != 16 || sig_pos_res->slot_source_offset != 32 ||
			sig_pos_res->slot_wrapper_ptr_offset != 48)
		{
			std::cerr << "Test 16 failed: Signal dynamic layout resolution\n";
			return 16;
		}

		// --- Signal Negatives ---
		// 1. Unanchored straight-line Signal stores
		std::vector<std::uint8_t> sig_code;
		const auto sig_xref_off = sig_code.size();
		const auto sig_target_disp = static_cast<std::int32_t>(vft_addr - (code_addr + sig_xref_off + 7));
		sig_code.insert(sig_code.end(), {0x4C, 0x8D, 0x1D});
		const auto* sdisp_bytes = reinterpret_cast<const std::uint8_t*>(&sig_target_disp);
		sig_code.insert(sig_code.end(), sdisp_bytes, sdisp_bytes + 4);
		sig_code.insert(sig_code.end(), {0x48, 0x89, 0xCB});
		sig_code.insert(sig_code.end(), {0x49, 0x89, 0xD4});
		sig_code.insert(sig_code.end(), {0x4D, 0x89, 0xC5});
		sig_code.insert(sig_code.end(), {0x4D, 0x89, 0xCE});
		sig_code.insert(sig_code.end(), {0xB9, 0x40, 0x00, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0xE8, 0x00, 0x00, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0x48, 0x89, 0x43, 0x08});
		sig_code.insert(sig_code.end(), {0xC7, 0x40, 0x00, 0x01, 0x00, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0xC7, 0x40, 0x04, 0x01, 0x00, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0x4C, 0x8D, 0x0D, 0x00, 0x01, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0x4C, 0x89, 0x48, 0x08});
		sig_code.insert(sig_code.end(), {0x48, 0x89, 0x40, 0x10});
		sig_code.insert(sig_code.end(), {0xC7, 0x40, 0x18, 0x00, 0x00, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0x4C, 0x89, 0x60, 0x20});
		sig_code.insert(sig_code.end(), {0x4C, 0x8D, 0x0D, 0x00, 0x02, 0x00, 0x00});
		sig_code.insert(sig_code.end(), {0x4C, 0x89, 0x48, 0x28});
		sig_code.insert(sig_code.end(), {0x4C, 0x89, 0x68, 0x30});
		sig_code.insert(sig_code.end(), {0x4C, 0x89, 0x70, 0x38});
		sig_code.push_back(0xC3);

		const TestRuntimeFunction sig_pdata{
			.begin_address = 0x1000,
			.end_address = static_cast<std::uint32_t>(0x1000 + sig_code.size()),
			.unwind_info_address = 1,
		};

		const auto sig_res = rml::roblox::internals::resolve_signal_layout(
			std::as_bytes(std::span{sig_code}),
			code_addr,
			std::as_bytes(std::span{&sig_pdata, 1}),
			base_addr);

		if (sig_res)
		{
			std::cerr << "Test 16 failed: unanchored straight-line Signal stores were accepted\n";
			return 16;
		}

		// 2. Duplicate Signal insert signature (ambiguous evidence)
		std::vector<std::uint8_t> sig_dup_code = sig_pos_code;
		sig_dup_code.insert(sig_dup_code.end(), {
			0x40, 0x53, 0x56, 0x57, 0x48, 0x83, 0xEC, 0x30,
			0x48, 0x8B, 0xF2, 0x48, 0x8B, 0xF9, 0x33, 0xDB,
			0x89, 0x5C, 0x24, 0x20, 0xC3
		});
		const TestRuntimeFunction sig_dup_pdata{
			.begin_address = 0x1000,
			.end_address = static_cast<std::uint32_t>(0x1000 + sig_dup_code.size()),
			.unwind_info_address = 1,
		};
		const auto sig_dup_res = rml::roblox::internals::resolve_signal_layout(
			std::as_bytes(std::span{sig_dup_code}),
			code_addr,
			std::as_bytes(std::span{&sig_dup_pdata, 1}),
			base_addr,
			code_addr + off_disconnect,
			code_addr + off_disconnect);
		if (sig_dup_res)
		{
			std::cerr << "Test 16 failed: duplicate Signal insert signature did not fail closed\n";
			return 16;
		}

		// 3. Arbitrary CALL without size setup
		std::vector<std::uint8_t> sig_bad_call_code;
		const auto sbc_xref_off = sig_bad_call_code.size();
		const auto sbc_target_disp = static_cast<std::int32_t>(vft_addr - (code_addr + sbc_xref_off + 7));
		sig_bad_call_code.insert(sig_bad_call_code.end(), {0x4C, 0x8D, 0x05});
		const auto* sbc_disp_bytes = reinterpret_cast<const std::uint8_t*>(&sbc_target_disp);
		sig_bad_call_code.insert(sig_bad_call_code.end(), sbc_disp_bytes, sbc_disp_bytes + 4);
		sig_bad_call_code.insert(sig_bad_call_code.end(), {0xE8, 0x00, 0x00, 0x00, 0x00});
		sig_bad_call_code.push_back(0xC3);

		const TestRuntimeFunction sig_bad_pdata{
			.begin_address = 0x1000,
			.end_address = static_cast<std::uint32_t>(0x1000 + sig_bad_call_code.size()),
			.unwind_info_address = 1,
		};
		const auto sig_bad_res = rml::roblox::internals::resolve_signal_layout(
			std::as_bytes(std::span{sig_bad_call_code}), code_addr,
			std::as_bytes(std::span{&sig_bad_pdata, 1}), base_addr);
		if (sig_bad_res)
		{
			std::cerr << "Test 16 failed: Signal arbitrary call without size did not fail closed\n";
			return 16;
		}

		// --- Negative Fail Closed Checks ---
		const std::array dummy_code{std::byte{0xC3}};
		const auto dummy_fail = rml::roblox::internals::resolve_instance_layout(
			dummy_code, code_addr, {}, base_addr, vfts_span);
		if (dummy_fail)
		{
			std::cerr << "Test 16 failed: dummy RET without .pdata did not fail closed\n";
			return 16;
		}

		// --- Test 17: Diagnostic Sink Multi-Error Collection ---
		{
			std::vector<rml::roblox::internals::CompatibilityError> diagnostics;
			const auto res = rml::roblox::internals::resolve_reflection_layout(
				dummy_code, code_addr, get_string_atom_target,
				std::as_bytes(std::span{&sig_bad_pdata, 1}),
				base_addr, full_vft_sets, &diagnostics);
			if (res.has_value())
			{
				std::cerr << "Test 17 failed: expected failure on dummy code\n";
				return 17;
			}
			if (diagnostics.empty())
			{
				std::cerr << "Test 17 failed: expected collected diagnostics sink to contain errors\n";
				return 17;
			}
		}
	}

	std::cout << "All reflection layout resolver and capabilities tests passed successfully.\n";
	return 0;
}
