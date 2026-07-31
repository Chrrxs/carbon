#include "RobloxModLoader/roblox/datamodel_layout_resolver.hpp"
#include "RobloxModLoader/roblox/reflection/event.hpp"
#include "RobloxModLoader/roblox/reflection/runtime_layout_resolver.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/job_types.hpp"
#include "RobloxModLoader/memory/symbol_resolver.hpp"

#include <algorithm>
#include <charconv>
#include <cctype>
#include <array>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iostream>
#include <limits>
#include <map>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <vector>
#include <unordered_map>

namespace
{
	using rml::roblox::internals::CompatibilityError;
	using rml::roblox::internals::CompatibilityFailure;
	using rml::roblox::internals::DataModelLayoutEvidence;
	using rml::roblox::internals::InstanceLayoutEvidence;
	using rml::roblox::internals::JobLayoutEvidence;
	using rml::roblox::internals::ReflectionLayoutEvidence;
	using rml::roblox::internals::ReflectionVftSets;
	using rml::roblox::internals::SignalLayoutEvidence;

#pragma pack(push, 1)
	struct DOSHeader
	{
		std::uint16_t e_magic;
		std::uint8_t e_stub[58];
		std::uint32_t e_lfanew;
	};

	struct FileHeader
	{
		std::uint16_t machine;
		std::uint16_t number_of_sections;
		std::uint32_t time_date_stamp;
		std::uint32_t pointer_to_symbol_table;
		std::uint32_t number_of_symbols;
		std::uint16_t size_of_optional_header;
		std::uint16_t characteristics;
	};

	struct OptionalHeader64
	{
		std::uint16_t magic;
		std::uint8_t major_linker_version;
		std::uint8_t minor_linker_version;
		std::uint32_t size_of_code;
		std::uint32_t size_of_initialized_data;
		std::uint32_t size_of_uninitialized_data;
		std::uint32_t address_of_entry_point;
		std::uint32_t base_of_code;
		std::uint64_t image_base;
		std::uint32_t section_alignment;
		std::uint32_t file_alignment;
		std::uint16_t major_operating_system_version;
		std::uint16_t minor_operating_system_version;
		std::uint16_t major_image_version;
		std::uint16_t minor_image_version;
		std::uint16_t major_subsystem_version;
		std::uint16_t minor_subsystem_version;
		std::uint32_t win32_version_value;
		std::uint32_t size_of_image;
		std::uint32_t size_of_headers;
		std::uint32_t checksum;
		std::uint16_t subsystem;
		std::uint16_t dll_characteristics;
		std::uint64_t size_of_stack_reserve;
		std::uint64_t size_of_stack_commit;
		std::uint64_t size_of_heap_reserve;
		std::uint64_t size_of_heap_commit;
		std::uint32_t loader_flags;
		std::uint32_t number_of_rva_and_sizes;
	};

	struct SectionHeader
	{
		std::uint8_t name[8];
		std::uint32_t virtual_size;
		std::uint32_t virtual_address;
		std::uint32_t size_of_raw_data;
		std::uint32_t pointer_to_raw_data;
		std::uint32_t pointer_to_relocations;
		std::uint32_t pointer_to_line_numbers;
		std::uint16_t number_of_relocations;
		std::uint16_t number_of_line_numbers;
		std::uint32_t characteristics;
	};

	struct CompleteObjectLocator
	{
		std::uint32_t signature;
		std::uint32_t offset;
		std::uint32_t constructor_displacement;
		std::uint32_t type_descriptor_rva;
		std::uint32_t class_descriptor_rva;
		std::uint32_t self_rva;
	};
	struct ClassHierarchyDescriptor
	{
		std::uint32_t signature;
		std::uint32_t attributes;
		std::uint32_t num_base_classes;
		std::uint32_t base_class_array_rva;
	};

	struct BaseClassDescriptor
	{
		std::int32_t type_descriptor_rva;
		std::uint32_t num_contained_bases;
		std::int32_t member_displacement[3];
		std::uint32_t attributes;
		std::uint32_t class_hierarchy_rva;
	};

#pragma pack(pop)

	struct OfflineRTTILocatorInfo
	{
		std::string demangled_name;
		std::uint32_t type_descriptor_rva;
		std::vector<std::uint32_t> base_type_descriptor_rvas;
		std::vector<std::pair<std::string, std::ptrdiff_t>> base_class_offsets;
	};

	struct OfflineRTTIIndex
	{
		std::map<std::string, std::vector<std::uintptr_t>> class_vft_map;
		std::vector<std::pair<std::uintptr_t, OfflineRTTILocatorInfo>> vft_entries;
	};

	struct SectionSpan
	{
		std::string name;
		std::uint32_t rva;
		std::uint32_t virtual_size;
		std::uint32_t file_offset;
		std::uint32_t file_size;
	};

	struct MappedPE
	{
		std::uint64_t image_base{0};
		std::uint32_t size_of_image{0};
		std::vector<std::byte> image_bytes;
		std::vector<SectionSpan> sections;

		[[nodiscard]] const SectionSpan* find_section(std::string_view name) const
		{
			for (const auto& sec : sections)
			{
				if (sec.name == name)
					return &sec;
			}
			return nullptr;
		}

		[[nodiscard]] std::vector<const SectionSpan*> find_sections(std::string_view name) const
		{
			std::vector<const SectionSpan*> result;
			for (const auto& sec : sections)
			{
				if (sec.name == name)
					result.push_back(&sec);
			}
			return result;
		}

		[[nodiscard]] bool is_valid_rva(std::uint32_t rva, std::size_t size = 1) const
		{
			return rva < image_bytes.size() && size <= image_bytes.size() - rva;
		}

		[[nodiscard]] const std::byte* rva_to_ptr(std::uint32_t rva) const
		{
			if (!is_valid_rva(rva))
				return nullptr;
			return image_bytes.data() + rva;
		}
	};

	std::string failure_to_string(const CompatibilityFailure f)
	{
		switch (f)
		{
		case CompatibilityFailure::missing_signature:
			return "missing_signature";
		case CompatibilityFailure::insufficient_evidence:
			return "insufficient_evidence";
		case CompatibilityFailure::ambiguous_evidence:
			return "ambiguous_evidence";
		case CompatibilityFailure::unsupported_instruction_form:
			return "unsupported_instruction_form";
		case CompatibilityFailure::invalid_address_range:
			return "invalid_address_range";
		default:
			return "unknown_failure";
		}
	}

	void print_failure(
		const std::string_view exe_path,
		const CompatibilityError& err,
		const std::size_t supporting_calls = 0)
	{
		std::cout << "[FAIL] exe=" << exe_path
				  << " capability=" << err.capability
				  << " failure=" << failure_to_string(err.failure)
				  << " (numeric=" << static_cast<int>(err.failure) << ")"
				  << " matched_calls=" << err.matched_calls
				  << " decoded_candidates=" << err.decoded_candidates
				  << " supporting_calls=" << supporting_calls
				  << "\n";
	}

	std::optional<MappedPE> load_pe(const std::string& path)
	{
		std::ifstream file(path, std::ios::binary | std::ios::ate);
		if (!file.is_open())
		{
			return std::nullopt;
		}
		const auto file_size = static_cast<std::size_t>(file.tellg());
		if (file_size < sizeof(DOSHeader))
		{
			return std::nullopt;
		}
		std::vector<std::byte> raw_file(file_size);
		file.seekg(0, std::ios::beg);
		file.read(reinterpret_cast<char*>(raw_file.data()), file_size);

		const auto* dos = reinterpret_cast<const DOSHeader*>(raw_file.data());
		if (dos->e_magic != 0x5A4D || dos->e_lfanew + sizeof(std::uint32_t) + sizeof(FileHeader) + sizeof(OptionalHeader64) > file_size)
		{
			return std::nullopt;
		}

		const auto* pe_sig = reinterpret_cast<const std::uint32_t*>(raw_file.data() + dos->e_lfanew);
		if (*pe_sig != 0x00004550) // 'PE\0\0'
		{
			return std::nullopt;
		}

		const auto* file_header = reinterpret_cast<const FileHeader*>(raw_file.data() + dos->e_lfanew + 4);
		if (file_header->machine != 0x8664) // AMD64
		{
			return std::nullopt;
		}

		const auto* opt_header = reinterpret_cast<const OptionalHeader64*>(raw_file.data() + dos->e_lfanew + 4 + sizeof(FileHeader));
		if (opt_header->magic != 0x20B) // PE32+
		{
			return std::nullopt;
		}

		MappedPE pe;
		pe.image_base = opt_header->image_base;
		pe.size_of_image = opt_header->size_of_image;
		pe.image_bytes.resize(pe.size_of_image, std::byte{0});

		const auto section_table_offset = dos->e_lfanew + 4 + sizeof(FileHeader) + file_header->size_of_optional_header;
		if (section_table_offset + file_header->number_of_sections * sizeof(SectionHeader) > file_size)
		{
			return std::nullopt;
		}

		const auto* sections = reinterpret_cast<const SectionHeader*>(raw_file.data() + section_table_offset);
		for (std::uint16_t i = 0; i < file_header->number_of_sections; ++i)
		{
			const auto& sec = sections[i];
			char name_buf[9] = {0};
			std::memcpy(name_buf, sec.name, 8);
			const std::string name(name_buf);

			SectionSpan span{
				.name = name,
				.rva = sec.virtual_address,
				.virtual_size = sec.virtual_size,
				.file_offset = sec.pointer_to_raw_data,
				.file_size = sec.size_of_raw_data,
			};
			pe.sections.push_back(span);

			if (sec.pointer_to_raw_data < file_size)
			{
				const auto copy_size = std::min<std::size_t>(sec.size_of_raw_data, file_size - sec.pointer_to_raw_data);
				const auto target_size = std::min<std::size_t>(copy_size, pe.size_of_image - std::min<std::size_t>(sec.virtual_address, pe.size_of_image));
				if (sec.virtual_address < pe.size_of_image && target_size > 0)
				{
					std::memcpy(pe.image_bytes.data() + sec.virtual_address, raw_file.data() + sec.pointer_to_raw_data, target_size);
				}
			}
		}

		return pe;
	}

	std::uintptr_t pattern_scan_get_string_atom(
		std::span<const std::byte> code,
		const std::uintptr_t code_address,
		std::size_t& match_count)
	{
		// Signature: "48 89 5C 24 ? 57 48 83 EC 20 48 8B 1D ? ? ? ? 48 8B F9 48 85 DB"
		constexpr std::array<std::optional<std::uint8_t>, 23> pattern = {
			std::uint8_t{0x48}, std::uint8_t{0x89}, std::uint8_t{0x5C}, std::uint8_t{0x24}, std::nullopt,
			std::uint8_t{0x57}, std::uint8_t{0x48}, std::uint8_t{0x83}, std::uint8_t{0xEC}, std::uint8_t{0x20},
			std::uint8_t{0x48}, std::uint8_t{0x8B}, std::uint8_t{0x1D}, std::nullopt, std::nullopt, std::nullopt, std::nullopt,
			std::uint8_t{0x48}, std::uint8_t{0x8B}, std::uint8_t{0xF9}, std::uint8_t{0x48}, std::uint8_t{0x85}, std::uint8_t{0xDB}
		};

		match_count = 0;
		std::uintptr_t matched_addr = 0;

		if (code.size() < pattern.size())
			return 0;

		for (std::size_t i = 0; i <= code.size() - pattern.size(); ++i)
		{
			std::uint32_t prefix = 0;
			std::memcpy(&prefix, code.data() + i, sizeof(prefix));
			if (prefix != 0x245C8948)
				continue;
			bool match = true;
			for (std::size_t j = 0; j < pattern.size(); ++j)
			{
				if (pattern[j].has_value() && static_cast<std::uint8_t>(code[i + j]) != pattern[j].value())
				{
					match = false;
					break;
				}
			}
			if (match)
			{
				++match_count;
				matched_addr = code_address + i;
			}
		}

		return match_count == 1 ? matched_addr : 0;
	}
	std::uintptr_t pattern_scan_signal_disconnect(
		std::span<const std::byte> code,
		const std::uintptr_t code_address,
		std::size_t& match_count)
	{
		// Signature: 48 89 5C 24 ? 57 48 83 EC 30 48 8B F9 33 DB 48 89 5C 24 ? E8 ? ? ? ? 48 89 44 24 ? 88 5C 24 ? 48 8B C8 E8 ? ? ? ? 85 C0 0F 85
		constexpr std::array<std::optional<std::uint8_t>, 46> pattern = {
			std::uint8_t{0x48}, std::uint8_t{0x89}, std::uint8_t{0x5C}, std::uint8_t{0x24}, std::nullopt,
			std::uint8_t{0x57}, std::uint8_t{0x48}, std::uint8_t{0x83}, std::uint8_t{0xEC}, std::uint8_t{0x30},
			std::uint8_t{0x48}, std::uint8_t{0x8B}, std::uint8_t{0xF9}, std::uint8_t{0x33}, std::uint8_t{0xDB},
			std::uint8_t{0x48}, std::uint8_t{0x89}, std::uint8_t{0x5C}, std::uint8_t{0x24}, std::nullopt,
			std::uint8_t{0xE8}, std::nullopt, std::nullopt, std::nullopt, std::nullopt,
			std::uint8_t{0x48}, std::uint8_t{0x89}, std::uint8_t{0x44}, std::uint8_t{0x24}, std::nullopt,
			std::uint8_t{0x88}, std::uint8_t{0x5C}, std::uint8_t{0x24}, std::nullopt, std::uint8_t{0x48},
			std::uint8_t{0x8B}, std::uint8_t{0xC8}, std::uint8_t{0xE8}, std::nullopt, std::nullopt, std::nullopt, std::nullopt,
			std::uint8_t{0x85}, std::uint8_t{0xC0}, std::uint8_t{0x0F}, std::uint8_t{0x85}
		};

		match_count = 0;
		std::uintptr_t matched_addr = 0;
		if (code.size() < pattern.size())
			return 0;

		for (std::size_t i = 0; i <= code.size() - pattern.size(); ++i)
		{
			std::uint32_t prefix = 0;
			std::memcpy(&prefix, code.data() + i, sizeof(prefix));
			if (prefix != 0x245C8948)
				continue;
			bool match = true;
			for (std::size_t j = 0; j < pattern.size(); ++j)
			{
				if (pattern[j].has_value() && static_cast<std::uint8_t>(code[i + j]) != pattern[j].value())
				{
					match = false;
					break;
				}
			}
			if (match)
			{
				++match_count;
				matched_addr = code_address + i;
			}
		}
		return match_count == 1 ? matched_addr : 0;
	}

	std::uintptr_t pattern_scan_signal_slot_free(
		std::span<const std::byte> code,
		const std::uintptr_t code_address,
		std::size_t& match_count)
	{
		// Signature: 48 89 5C 24 10 48 89 74 24 18 57 48 83 EC 20 48 8B D9 E8 ? ? ? ? 48 8D 50 10 BF FF FF FF FF 48 3B DA 0F 82
		constexpr std::array<std::optional<std::uint8_t>, 37> pattern = {
			std::uint8_t{0x48}, std::uint8_t{0x89}, std::uint8_t{0x5C}, std::uint8_t{0x24}, std::uint8_t{0x10},
			std::uint8_t{0x48}, std::uint8_t{0x89}, std::uint8_t{0x74}, std::uint8_t{0x24}, std::uint8_t{0x18},
			std::uint8_t{0x57}, std::uint8_t{0x48}, std::uint8_t{0x83}, std::uint8_t{0xEC}, std::uint8_t{0x20},
			std::uint8_t{0x48}, std::uint8_t{0x8B}, std::uint8_t{0xD9}, std::uint8_t{0xE8}, std::nullopt, std::nullopt, std::nullopt, std::nullopt,
			std::uint8_t{0x48}, std::uint8_t{0x8D}, std::uint8_t{0x50}, std::uint8_t{0x10}, std::uint8_t{0xBF},
			std::uint8_t{0xFF}, std::uint8_t{0xFF}, std::uint8_t{0xFF}, std::uint8_t{0xFF}, std::uint8_t{0x48},
			std::uint8_t{0x3B}, std::uint8_t{0xDA}, std::uint8_t{0x0F}, std::uint8_t{0x82}
		};

		match_count = 0;
		std::uintptr_t matched_addr = 0;
		if (code.size() < pattern.size())
			return 0;

		for (std::size_t i = 0; i <= code.size() - pattern.size(); ++i)
		{
			std::uint32_t prefix = 0;
			std::memcpy(&prefix, code.data() + i, sizeof(prefix));
			if (prefix != 0x245C8948)
				continue;
			bool match = true;
			for (std::size_t j = 0; j < pattern.size(); ++j)
			{
				if (pattern[j].has_value() && static_cast<std::uint8_t>(code[i + j]) != pattern[j].value())
				{
					match = false;
					break;
				}
			}
			if (match)
			{
				++match_count;
				matched_addr = code_address + i;
			}
		}
		return match_count == 1 ? matched_addr : 0;
	}


	bool name_matches(std::string_view demangled, std::string_view target)
	{
		if (demangled == target)
			return true;
		if (demangled.starts_with("class ") && demangled.substr(6) == target)
			return true;
		if (demangled.starts_with("struct ") && demangled.substr(7) == target)
			return true;
		return false;
	}

	std::string demangle_type_descriptor_name(const char* mangled)
	{
		if (!mangled || !*mangled)
			return {};
		const char* p = mangled;
		if (*p == '.')
			++p;
		std::string result = rml::memory::demangle(p);
		if (result.empty())
			result = p;
		return result;
	}

	OfflineRTTIIndex scan_msvc_x64_rtti_vfts(const MappedPE& pe)
	{
		OfflineRTTIIndex rtti_index;
		const auto rdata_secs = pe.find_sections(".rdata");
		if (rdata_secs.empty())
			return rtti_index;

		std::unordered_map<std::uint64_t, OfflineRTTILocatorInfo> locator_infos;
		std::size_t locator_reserve = 0;
		for (const auto* sec : rdata_secs)
			locator_reserve += sec->virtual_size / 96;
		locator_infos.reserve(locator_reserve);
		std::uint64_t minimum_locator_address =
			(std::numeric_limits<std::uint64_t>::max)();
		std::uint64_t maximum_locator_address = 0;
		for (const auto* sec : rdata_secs)
		{
			if (sec->virtual_size < sizeof(CompleteObjectLocator))
				continue;

			const auto end_rva = sec->rva + sec->virtual_size - sizeof(CompleteObjectLocator);
			for (std::uint32_t rva = sec->rva; rva <= end_rva; rva += 4)
			{
				const auto* col = reinterpret_cast<const CompleteObjectLocator*>(pe.rva_to_ptr(rva));
				if (!col || col->signature != 1 || col->self_rva != rva ||
					!pe.is_valid_rva(col->type_descriptor_rva, 17))
					continue;

				const auto* td_ptr = pe.rva_to_ptr(col->type_descriptor_rva);
				const auto demangled = demangle_type_descriptor_name(
					reinterpret_cast<const char*>(td_ptr + 16));
				if (demangled.empty())
					continue;

				OfflineRTTILocatorInfo info;
				info.demangled_name = demangled;
				info.type_descriptor_rva = col->type_descriptor_rva;

				if (pe.is_valid_rva(col->class_descriptor_rva, sizeof(ClassHierarchyDescriptor)))
				{
					const auto* chd = reinterpret_cast<const ClassHierarchyDescriptor*>(pe.rva_to_ptr(col->class_descriptor_rva));
					if (chd && chd->signature <= 1 && chd->num_base_classes > 0 && chd->num_base_classes <= 100)
					{
						if (pe.is_valid_rva(chd->base_class_array_rva, chd->num_base_classes * sizeof(std::uint32_t)))
						{
							for (std::uint32_t i = 0; i < chd->num_base_classes; ++i)
							{
								const auto bcd_rva_ptr = pe.rva_to_ptr(chd->base_class_array_rva + i * sizeof(std::uint32_t));
								std::uint32_t bcd_rva = 0;
								std::memcpy(&bcd_rva, bcd_rva_ptr, sizeof(bcd_rva));

								if (!pe.is_valid_rva(bcd_rva, sizeof(BaseClassDescriptor)))
								{
									info.base_type_descriptor_rvas.clear();
									info.base_class_offsets.clear();
									break;
								}
								const auto* bcd = reinterpret_cast<const BaseClassDescriptor*>(pe.rva_to_ptr(bcd_rva));
								const auto base_td_rva = static_cast<std::uint32_t>(bcd->type_descriptor_rva);
								if (!pe.is_valid_rva(base_td_rva, 17))
								{
									info.base_type_descriptor_rvas.clear();
									info.base_class_offsets.clear();
									break;
								}
								info.base_type_descriptor_rvas.push_back(base_td_rva);
								if (bcd->member_displacement[1] == -1 &&
									bcd->member_displacement[2] == 0)
								{
									const auto base_name = demangle_type_descriptor_name(
										reinterpret_cast<const char*>(pe.rva_to_ptr(base_td_rva) + 16));
									if (!base_name.empty())
										info.base_class_offsets.push_back(
											{base_name, bcd->member_displacement[0]});
								}
							}
						}
					}
				}

				const auto locator_address = pe.image_base + rva;
				if (locator_infos.emplace(locator_address, std::move(info)).second)
				{
					minimum_locator_address = (std::min)(
						minimum_locator_address, locator_address);
					maximum_locator_address = (std::max)(
						maximum_locator_address, locator_address);
				}
			}
		}
		if (locator_infos.empty())
			return rtti_index;

		for (const auto* sec : rdata_secs)
		{
			if (sec->virtual_size < sizeof(std::uint64_t))
				continue;

			const auto end_rva = sec->rva + sec->virtual_size - sizeof(std::uint64_t);
			for (std::uint32_t rva = sec->rva; rva <= end_rva; rva += sizeof(std::uint64_t))
			{
				std::uint64_t value = 0;
				std::memcpy(&value, pe.rva_to_ptr(rva), sizeof(value));
				if (value < minimum_locator_address ||
					value > maximum_locator_address ||
					(value & 0x3) != 0)
				{
					continue;
				}
				const auto locator = locator_infos.find(value);
				if (locator != locator_infos.end())
				{
					const std::uintptr_t vft_addr = static_cast<std::uintptr_t>(pe.image_base + rva + sizeof(std::uint64_t));
					rtti_index.class_vft_map[locator->second.demangled_name].push_back(vft_addr);
					rtti_index.vft_entries.push_back({vft_addr, locator->second});
				}
			}
		}

		return rtti_index;
	}

	std::vector<std::uintptr_t> gather_event_family_rtti_vfts(
		const OfflineRTTIIndex& rtti_index,
		std::string_view target1,
		std::string_view target2 = {})
	{
		std::vector<std::uint32_t> target_td_rvas;
		for (const auto& [vft, info] : rtti_index.vft_entries)
		{
			if (name_matches(info.demangled_name, target1) || (!target2.empty() && name_matches(info.demangled_name, target2)))
			{
				target_td_rvas.push_back(info.type_descriptor_rva);
			}
		}
		if (target_td_rvas.empty())
			return {};

		std::sort(target_td_rvas.begin(), target_td_rvas.end());
		target_td_rvas.erase(std::unique(target_td_rvas.begin(), target_td_rvas.end()), target_td_rvas.end());

		std::vector<std::uintptr_t> vfts;
		for (const auto& [vft, info] : rtti_index.vft_entries)
		{
			bool match = false;
			for (const auto target_rva : target_td_rvas)
			{
				if (info.type_descriptor_rva == target_rva)
				{
					match = true;
					break;
				}
				for (const auto base_rva : info.base_type_descriptor_rvas)
				{
					if (base_rva == target_rva)
					{
						match = true;
						break;
					}
				}
				if (match)
					break;
			}
			if (match)
			{
				vfts.push_back(vft);
			}
		}

		std::sort(vfts.begin(), vfts.end());
		vfts.erase(std::unique(vfts.begin(), vfts.end()), vfts.end());
		return vfts;
	}

	std::vector<std::uintptr_t> gather_rtti_vfts(
		const std::map<std::string, std::vector<std::uintptr_t>>& class_vft_map,
		std::string_view target1,
		std::string_view target2 = {})
	{
		std::vector<std::uintptr_t> vfts;
		for (const auto& [name, vec] : class_vft_map)
		{
			if (name_matches(name, target1) || (!target2.empty() && name_matches(name, target2)))
			{
				vfts.insert(vfts.end(), vec.begin(), vec.end());
			}
		}
		std::sort(vfts.begin(), vfts.end());
		vfts.erase(std::unique(vfts.begin(), vfts.end()), vfts.end());
		return vfts;
	}
	enum class Stage : std::uint32_t
	{
		None = 0,
		RTTI = 1 << 0,
		Reflection = 1 << 1,
		DataModel = 1 << 2,
		Instance = 1 << 3,
		Signal = 1 << 4,
		Job = 1 << 5,
		All = RTTI | Reflection | DataModel | Instance | Signal | Job
	};

	constexpr Stage operator|(Stage a, Stage b) noexcept
	{
		return static_cast<Stage>(static_cast<std::uint32_t>(a) | static_cast<std::uint32_t>(b));
	}

	constexpr Stage operator&(Stage a, Stage b) noexcept
	{
		return static_cast<Stage>(static_cast<std::uint32_t>(a) & static_cast<std::uint32_t>(b));
	}

	constexpr Stage& operator|=(Stage& a, Stage b) noexcept
	{
		a = a | b;
		return a;
	}

	constexpr bool has_stage(Stage mask, Stage stage) noexcept
	{
		return (static_cast<std::uint32_t>(mask) & static_cast<std::uint32_t>(stage)) != 0;
	}

	std::optional<Stage> parse_stage_name(std::string_view name)
	{
		std::string lower_name;
		lower_name.reserve(name.size());
		for (char c : name)
		{
			lower_name.push_back(static_cast<char>(std::tolower(static_cast<unsigned char>(c))));
		}

		if (lower_name == "rtti" || lower_name == "rttiextraction")
			return Stage::RTTI;
		if (lower_name == "reflection" || lower_name == "reflectionlayout")
			return Stage::Reflection;
		if (lower_name == "datamodel" || lower_name == "datamodellayout")
			return Stage::DataModel;
		if (lower_name == "instance" || lower_name == "instancelayout")
			return Stage::Instance;
		if (lower_name == "signal" || lower_name == "signallayout")
			return Stage::Signal;
		if (lower_name == "job" || lower_name == "joblayout")
			return Stage::Job;

		return std::nullopt;
	}

	struct CommandLineOptions
	{
		Stage selected_stages{Stage::None};
		std::vector<std::string> executables;
		bool run_self_test{false};
		bool parse_success{true};
		std::string error_message;
		bool has_trace_signal_connect{false};
		std::uintptr_t trace_signal_connect_focus{0};
	};

	void print_usage(std::ostream& os)
	{
		os << "Usage: offline_abi_resolver_tests [--stage <RTTI|Reflection|DataModel|Instance|Signal|Job>]... [--trace-signal-connect <0xVA>] <exe>...\n";
	}

	std::optional<std::uintptr_t> parse_address(const std::string_view str)
	{
		if (str.empty())
			return std::nullopt;

		std::size_t idx = 0;
		int base = 10;
		if (str.starts_with("0x") || str.starts_with("0X"))
		{
			base = 16;
			idx = 2;
			if (idx == str.size())
				return std::nullopt;
		}
		else
		{
			bool all_digits = true;
			for (const char c : str)
			{
				if (!std::isdigit(static_cast<unsigned char>(c)))
				{
					all_digits = false;
					break;
				}
			}
			base = all_digits ? 10 : 16;
		}

		std::uintptr_t val = 0;
		const char* first = str.data() + idx;
		const char* last = str.data() + str.size();
		const auto [ptr, ec] = std::from_chars(first, last, val, base);
		if (ec != std::errc{} || ptr != last)
			return std::nullopt;

		return val;
	}

	CommandLineOptions parse_command_line(const int argc, const char* const* argv)
	{
		CommandLineOptions opts;
		bool stage_flag_used = false;

		for (int i = 1; i < argc; ++i)
		{
			const std::string_view arg = argv[i];
			if (arg == "--stage")
			{
				if (i + 1 >= argc)
				{
					opts.parse_success = false;
					opts.error_message = "Error: --stage option requires a stage value.";
					return opts;
				}
				const std::string_view stage_str = argv[++i];
				const auto parsed = parse_stage_name(stage_str);
				if (!parsed.has_value())
				{
					opts.parse_success = false;
					opts.error_message = "Error: Unknown stage name '" + std::string(stage_str) + "'.";
					return opts;
				}
				stage_flag_used = true;
				opts.selected_stages |= *parsed;
			}
			else if (arg == "--trace-signal-connect")
			{
				if (i + 1 >= argc)
				{
					opts.parse_success = false;
					opts.error_message = "Error: --trace-signal-connect option requires an address value.";
					return opts;
				}
				const std::string_view addr_str = argv[++i];
				const auto parsed_addr = parse_address(addr_str);
				if (!parsed_addr.has_value())
				{
					opts.parse_success = false;
					opts.error_message = "Error: Invalid address format for --trace-signal-connect: '" + std::string(addr_str) + "'.";
					return opts;
				}
				opts.has_trace_signal_connect = true;
				stage_flag_used = true;
				opts.trace_signal_connect_focus = *parsed_addr;
				opts.selected_stages |= Stage::Signal;
			}
			else if (arg == "--self-test" || arg == "--test")
			{
				opts.run_self_test = true;
			}
			else if (arg.starts_with("--"))
			{
				opts.parse_success = false;
				opts.error_message = "Error: Unknown option '" + std::string(arg) + "'.";
				return opts;
			}
			else
			{
				opts.executables.push_back(std::string(arg));
			}
		}

		if (!opts.run_self_test)
		{
			if (opts.executables.empty())
			{
				opts.parse_success = false;
				opts.error_message = "Error: No executable specified.";
				return opts;
			}
		}

		if (!stage_flag_used)
		{
			opts.selected_stages = Stage::All;
		}

		return opts;
	}

	int run_cli_tests()
	{
		int failures = 0;

		const auto check = [&failures](bool cond, const char* msg) {
			if (!cond)
			{
				std::cerr << "[FAIL] CLI test assertion failed: " << msg << "\n";
				++failures;
			}
		};

		// 1. Default selection: no --stage provided, executable provided.
		{
			const char* argv[] = {"offline_abi_resolver_tests", "dummy.exe"};
			const auto opts = parse_command_line(2, argv);
			check(opts.parse_success, "Default selection parse success");
			check(opts.executables.size() == 1 && opts.executables[0] == "dummy.exe", "Default selection executable");
			check(opts.selected_stages == Stage::All, "Default selection selects all stages");
			check(has_stage(opts.selected_stages, Stage::RTTI), "Default has RTTI");
			check(has_stage(opts.selected_stages, Stage::Reflection), "Default has Reflection");
			check(has_stage(opts.selected_stages, Stage::DataModel), "Default has DataModel");
			check(has_stage(opts.selected_stages, Stage::Instance), "Default has Instance");
			check(has_stage(opts.selected_stages, Stage::Signal), "Default has Signal");
			check(has_stage(opts.selected_stages, Stage::Job), "Default has Job");
		}

		// 2. Focused selection: --stage Instance exe
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage", "Instance", "dummy.exe"};
			const auto opts = parse_command_line(4, argv);
			check(opts.parse_success, "Focused selection parse success");
			check(opts.selected_stages == Stage::Instance, "Focused selection selects Instance only");
			check(has_stage(opts.selected_stages, Stage::Instance), "Focused has Instance");
			check(!has_stage(opts.selected_stages, Stage::Reflection), "Focused does not have Reflection");
			check(!has_stage(opts.selected_stages, Stage::DataModel), "Focused does not have DataModel");
			check(!has_stage(opts.selected_stages, Stage::Signal), "Focused does not have Signal");
			check(!has_stage(opts.selected_stages, Stage::Job), "Focused does not have Job");
			check(!has_stage(opts.selected_stages, Stage::RTTI), "Focused stage mask does not include RTTI output flag");
		}

		// 3. Repeated selection: --stage Signal --stage Job exe1 exe2
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage", "Signal", "--stage", "Job", "exe1.exe", "exe2.exe"};
			const auto opts = parse_command_line(7, argv);
			check(opts.parse_success, "Repeated selection parse success");
			check(opts.executables.size() == 2 && opts.executables[0] == "exe1.exe" && opts.executables[1] == "exe2.exe", "Repeated selection executables");
			check(opts.selected_stages == (Stage::Signal | Stage::Job), "Repeated selection selects Signal and Job");
			check(has_stage(opts.selected_stages, Stage::Signal), "Repeated has Signal");
			check(has_stage(opts.selected_stages, Stage::Job), "Repeated has Job");
			check(!has_stage(opts.selected_stages, Stage::Instance), "Repeated does not have Instance");
		}

		// 4. Mixed-case values & stage name aliases
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage", "inSTancE", "--stage", "sigNAL", "--stage", "rttiextraction", "exe.exe"};
			const auto opts = parse_command_line(8, argv);
			check(opts.parse_success, "Mixed-case selection parse success");
			check(has_stage(opts.selected_stages, Stage::Instance), "Mixed-case has Instance");
			check(has_stage(opts.selected_stages, Stage::Signal), "Mixed-case has Signal");
			check(has_stage(opts.selected_stages, Stage::RTTI), "Mixed-case has RTTI");
			check(!has_stage(opts.selected_stages, Stage::Job), "Mixed-case does not have Job");
		}

		// 5. Invalid selection (unknown stage name)
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage", "NonExistentStage", "dummy.exe"};
			const auto opts = parse_command_line(4, argv);
			check(!opts.parse_success, "Unknown stage fails parse");
		}

		// 6. Missing value for --stage option
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage"};
			const auto opts = parse_command_line(2, argv);
			check(!opts.parse_success, "Missing stage value fails parse");
		}

		// 7. Missing executable (only flags)
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage", "Instance"};
			const auto opts = parse_command_line(3, argv);
			check(!opts.parse_success, "Missing executable fails parse");
		}

		// 8. Missing executable (no arguments)
		{
			const char* argv[] = {"offline_abi_resolver_tests"};
			const auto opts = parse_command_line(1, argv);
			check(!opts.parse_success, "No args fails parse");
		}

		// 9. Trace signal connect - valid hex with 0x
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--trace-signal-connect", "0x1414e9b60", "dummy.exe"};
			const auto opts = parse_command_line(4, argv);
			check(opts.parse_success, "Trace signal connect hex parse success");
			check(opts.has_trace_signal_connect, "Trace signal connect flag set");
			check(opts.trace_signal_connect_focus == 0x1414e9b60, "Trace signal connect focus parsed correctly");
			check(has_stage(opts.selected_stages, Stage::Signal), "Trace signal connect implies Signal stage");
			check(opts.selected_stages == Stage::Signal, "Trace signal connect selects only Signal stage");
		}

		// 10. Trace signal connect - valid decimal
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--trace-signal-connect", "5390637920", "dummy.exe"};
			const auto opts = parse_command_line(4, argv);
			check(opts.parse_success, "Trace signal connect decimal parse success");
			check(opts.has_trace_signal_connect, "Trace signal connect flag set");
			check(opts.trace_signal_connect_focus == 0x1414e9b60, "Trace signal connect decimal focus parsed correctly");
		}

		// 11. Trace signal connect - focus with restrictive stage
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--stage", "RTTI", "--trace-signal-connect", "0x1414e9b60", "dummy.exe"};
			const auto opts = parse_command_line(6, argv);
			check(opts.parse_success, "Trace signal connect with restrictive stage parse success");
			check(has_stage(opts.selected_stages, Stage::Signal), "Focus requires Signal stage even when stage flag restricts");
			check(has_stage(opts.selected_stages, Stage::RTTI), "RTTI stage preserved");
		}

		// 12. Trace signal connect - missing value
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--trace-signal-connect"};
			const auto opts = parse_command_line(2, argv);
			check(!opts.parse_success, "Trace signal connect missing value fails parse");
		}

		// 13. Trace signal connect - invalid address
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--trace-signal-connect", "0xinvalid", "dummy.exe"};
			const auto opts = parse_command_line(4, argv);
			check(!opts.parse_success, "Trace signal connect invalid address fails parse");
		}

		// 14. Trace signal connect - trailing garbage
		{
			const char* argv[] = {"offline_abi_resolver_tests", "--trace-signal-connect", "0x123xyz", "dummy.exe"};
			const auto opts = parse_command_line(4, argv);
			check(!opts.parse_success, "Trace signal connect trailing garbage fails parse");
		}

		if (failures == 0)
		{
			std::cout << "All offline ABI resolver CLI stage selection tests passed successfully!\n";
		}
		return failures;
	}
}

const rml::roblox::internals::RobloxInternalsProfile& get_roblox_internals_profile()
{
	std::abort();
}

const rml::roblox::internals::RobloxInternalsProfile* try_get_roblox_internals_profile() noexcept
{
	return nullptr;
}

namespace RBX::Reflection
{
	void EventSource::process_remote_event(const EventDescriptor&, const EventArguments&, const SystemAddress&)
	{
	}

	void EventSource::raise_event_invocation(const EventDescriptor&, const EventArguments&, const SystemAddress*)
	{
	}
}

int main(const int argc, char** argv)
{
	const auto opts = parse_command_line(argc, argv);
	if (!opts.parse_success)
	{
		print_usage(std::cerr);
		return 1;
	}

	if (opts.run_self_test)
	{
		return run_cli_tests();
	}

	bool any_failed = false;

	for (const auto& exe_path : opts.executables)
	{
		std::cout << "--- Processing: " << exe_path << " ---\n";
		auto pe_opt = load_pe(exe_path);
		if (!pe_opt.has_value())
		{
			any_failed = true;
			const CompatibilityError err{
				.capability = "Executable.Format",
				.failure = CompatibilityFailure::invalid_address_range,
			};
			print_failure(exe_path, err);
			continue;
		}

		const auto& pe = *pe_opt;
		const auto module_base = static_cast<std::uintptr_t>(pe.image_base);

		const auto text_sections = pe.find_sections(".text");
		std::optional<CompatibilityError> text_error;
		if (text_sections.empty())
		{
			text_error = CompatibilityError{
				.capability = "Executable.TextSection",
				.failure = CompatibilityFailure::missing_signature,
			};
		}
		else if (text_sections.size() > 1)
		{
			text_error = CompatibilityError{
				.capability = "Executable.TextSection",
				.failure = CompatibilityFailure::ambiguous_evidence,
			};
		}

		const auto pdata_sections = pe.find_sections(".pdata");
		std::optional<CompatibilityError> pdata_error;
		if (pdata_sections.empty())
		{
			pdata_error = CompatibilityError{
				.capability = "Executable.RuntimeFunctions",
				.failure = CompatibilityFailure::missing_signature,
			};
		}
		else if (pdata_sections.size() > 1)
		{
			pdata_error = CompatibilityError{
				.capability = "Executable.RuntimeFunctions",
				.failure = CompatibilityFailure::ambiguous_evidence,
			};
		}

		std::span<const std::byte> code;
		std::uintptr_t code_address = 0;
		if (!text_error.has_value() && !text_sections.empty())
		{
			const auto* text_sec = text_sections.front();
			code = std::span<const std::byte>(pe.image_bytes.data() + text_sec->rva, text_sec->virtual_size);
			code_address = module_base + text_sec->rva;
		}

		std::span<const std::byte> runtime_function_table;
		if (!pdata_error.has_value() && !pdata_sections.empty())
		{
			const auto* pdata_sec = pdata_sections.front();
			runtime_function_table = std::span<const std::byte>(pe.image_bytes.data() + pdata_sec->rva, pdata_sec->virtual_size);
		}

		std::size_t atom_matches = 0;
		std::size_t disconnect_matches = 0;
		std::size_t slot_free_matches = 0;
		std::uintptr_t get_string_atom_address = 0;
		std::uintptr_t signal_disconnect_address = 0;
		std::uintptr_t signal_slot_free_address = 0;
		if (!text_error.has_value())
		{
			get_string_atom_address = pattern_scan_get_string_atom(code, code_address, atom_matches);
			signal_disconnect_address = pattern_scan_signal_disconnect(code, code_address, disconnect_matches);
			signal_slot_free_address = pattern_scan_signal_slot_free(code, code_address, slot_free_matches);
		}

		if (has_stage(opts.selected_stages, Stage::RTTI))
		{
			std::cout << "[RUN] exe=" << exe_path << " stage=RTTIExtraction" << std::endl;
		}
		const auto rtti_index = scan_msvc_x64_rtti_vfts(pe);
		const auto gather_base_offset = [&](const std::string_view derived_name,
			const std::string_view base_name) -> std::optional<std::ptrdiff_t> {
			std::vector<std::ptrdiff_t> offsets;
			for (const auto& [vft, info] : rtti_index.vft_entries)
			{
				if (!name_matches(info.demangled_name, derived_name))
					continue;
				for (const auto& [candidate_base_name, offset] : info.base_class_offsets)
				{
					if (offset >= 0 && name_matches(candidate_base_name, base_name))
						offsets.push_back(offset);
				}
			}
			std::ranges::sort(offsets);
			offsets.erase(std::ranges::unique(offsets).begin(), offsets.end());
			return offsets.size() == 1
				? std::optional<std::ptrdiff_t>(offsets.front()) : std::nullopt;
		};
		const auto& class_vft_map = rtti_index.class_vft_map;

		const auto descriptor_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::Descriptor", "class RBX::Reflection::Descriptor");
		const auto member_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::MemberDescriptor", "class RBX::Reflection::MemberDescriptor");
		const auto property_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::PropertyDescriptor", "class RBX::Reflection::PropertyDescriptor");
		const auto function_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::FunctionDescriptor", "class RBX::Reflection::FunctionDescriptor");
		const auto type_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::Type", "class RBX::Reflection::Type");
		const auto yield_function_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::YieldFunctionDescriptor", "class RBX::Reflection::YieldFunctionDescriptor");
		const auto event_vfts = gather_event_family_rtti_vfts(rtti_index, "RBX::Reflection::EventDescriptor", "class RBX::Reflection::EventDescriptor");
		auto callback_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::SyncCallbackDescriptor", "class RBX::Reflection::SyncCallbackDescriptor");
		const auto async_callback_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::AsyncCallbackDescriptor", "class RBX::Reflection::AsyncCallbackDescriptor");
		callback_vfts.insert(callback_vfts.end(), async_callback_vfts.begin(), async_callback_vfts.end());
		const auto class_vfts = gather_rtti_vfts(class_vft_map, "RBX::Reflection::ClassDescriptor", "class RBX::Reflection::ClassDescriptor");
		const auto datamodel_vfts = gather_rtti_vfts(class_vft_map, "RBX::DataModel", "class RBX::DataModel");
		const auto instance_vfts = gather_rtti_vfts(class_vft_map, "RBX::Instance", "class RBX::Instance");
		const auto dm_job_vfts = gather_rtti_vfts(class_vft_map, "RBX::DataModelJob", "class RBX::DataModelJob");
		const auto waiting_job_vfts = gather_rtti_vfts(class_vft_map, "RBX::ScriptContextFacets::WaitingHybridScriptsJob", "class RBX::ScriptContextFacets::WaitingHybridScriptsJob");
		const auto datamodel_instance_base_offset =
			gather_base_offset("RBX::DataModel", "RBX::Instance");
		std::vector<std::uintptr_t> waiting_job_step_addresses;
		for (const auto vft : waiting_job_vfts)
		{
			const auto entry_rva = static_cast<std::uint32_t>(
				vft - module_base +
				rml::JobVtable::kWaitingHybridScriptsExecutionStepIndex * sizeof(std::uint64_t));
			if (!pe.is_valid_rva(entry_rva, sizeof(std::uint64_t)))
				continue;
			std::uint64_t step_address = 0;
			std::memcpy(&step_address, pe.rva_to_ptr(entry_rva), sizeof(step_address));
			if (step_address >= code_address && step_address < code_address + code.size() &&
				std::ranges::find(waiting_job_step_addresses, step_address) ==
					waiting_job_step_addresses.end())
			{
				waiting_job_step_addresses.push_back(static_cast<std::uintptr_t>(step_address));
			}
		}
		if (has_stage(opts.selected_stages, Stage::RTTI))
		{
			std::cout << "[EXTRACT] exe=" << exe_path
					  << " text_bytes=" << code.size()
					  << " pdata_bytes=" << runtime_function_table.size()
					  << " module_base=0x" << std::hex << module_base
					  << " get_string_atom=0x" << get_string_atom_address << std::dec
					  << " atom_matches=" << atom_matches
					  << " rtti_classes=" << class_vft_map.size()
					  << " vfts={descriptor:" << descriptor_vfts.size()
					  << ",member:" << member_vfts.size()
					  << ",property:" << property_vfts.size()
					  << ",function:" << function_vfts.size()
					  << ",type:" << type_vfts.size()
					  << ",yield:" << yield_function_vfts.size()
					  << ",event:" << event_vfts.size()
					  << ",callback:" << callback_vfts.size()
					  << ",class:" << class_vfts.size()
					  << ",datamodel:" << datamodel_vfts.size()
					  << ",instance:" << instance_vfts.size()
					  << ",datamodel_job:" << dm_job_vfts.size()
					  << ",waiting_job:" << waiting_job_vfts.size()
					  << "}" << std::endl;
		}

		std::vector<std::uintptr_t> instance_vft_entries;
		for (const auto vft : instance_vfts)
		{
			const auto vft_rva = static_cast<std::uint32_t>(vft - module_base);
			for (std::size_t i = 0; i < 64; ++i)
			{
				if (!pe.is_valid_rva(vft_rva + i * sizeof(std::uint64_t), sizeof(std::uint64_t)))
					break;
				std::uint64_t fn_va = 0;
				std::memcpy(&fn_va, pe.rva_to_ptr(vft_rva + i * sizeof(std::uint64_t)), sizeof(fn_va));
				if (fn_va >= code_address && fn_va < code_address + code.size())
				{
					instance_vft_entries.push_back(static_cast<std::uintptr_t>(fn_va));
				}
				else if (fn_va == 0)
				{
					break;
				}
			}
		}

		// Collect prerequisite / RTTI failures
		std::vector<CompatibilityError> prereq_errors;
		if (text_error.has_value()) prereq_errors.push_back(*text_error);
		if (pdata_error.has_value()) prereq_errors.push_back(*pdata_error);
		if (!text_error.has_value() && get_string_atom_address == 0)
		{
			prereq_errors.push_back(CompatibilityError{
				.capability = "Reflection.GetStringAtom",
				.failure = atom_matches > 1 ? CompatibilityFailure::ambiguous_evidence : CompatibilityFailure::missing_signature,
			});
		}
		if (descriptor_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.DescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (member_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.MemberDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (property_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.PropertyDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (function_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.FunctionDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (type_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.TypeRTTI", .failure = CompatibilityFailure::missing_signature});
		if (yield_function_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.YieldFunctionDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (event_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.EventDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (callback_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.CallbackDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (class_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Reflection.ClassDescriptorRTTI", .failure = CompatibilityFailure::missing_signature});
		if (datamodel_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "DataModel.RTTI", .failure = CompatibilityFailure::missing_signature});
		if (instance_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "Instance.RTTI", .failure = CompatibilityFailure::missing_signature});
		if (dm_job_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "DataModelJob.RTTI", .failure = CompatibilityFailure::missing_signature});
		if (waiting_job_vfts.empty()) prereq_errors.push_back(CompatibilityError{.capability = "WaitingHybridScriptsJob.RTTI", .failure = CompatibilityFailure::missing_signature});

		std::vector<CompatibilityError> file_diagnostics;
		if (has_stage(opts.selected_stages, Stage::RTTI))
		{
			for (const auto& err : prereq_errors)
			{
				file_diagnostics.push_back(err);
			}
		}
		const auto append_failure = [&file_diagnostics](
			const std::vector<CompatibilityError>& diagnostics,
			const CompatibilityError& terminal)
		{
			file_diagnostics.insert(file_diagnostics.end(), diagnostics.begin(), diagnostics.end());
			const auto terminal_present = std::any_of(
				diagnostics.begin(),
				diagnostics.end(),
				[&terminal](const CompatibilityError& diagnostic)
				{
					return diagnostic.capability == terminal.capability && diagnostic.failure == terminal.failure;
				});
			if (!terminal_present)
			{
				file_diagnostics.push_back(terminal);
			}
		};

		if (has_stage(opts.selected_stages, Stage::Reflection))
		{
		// 1. Reflection Layout
		{
			std::vector<CompatibilityError> refl_diag;
			const ReflectionVftSets vft_sets{
				.descriptor_vfts = descriptor_vfts,
				.member_vfts = member_vfts,
				.property_vfts = property_vfts,
				.function_vfts = function_vfts,
				.type_vfts = type_vfts,
				.yield_function_vfts = yield_function_vfts,
				.event_vfts = event_vfts,
				.callback_vfts = callback_vfts,
				.class_descriptor_vfts = class_vfts,
			};
			std::cout << "[RUN] exe=" << exe_path << " stage=ReflectionLayout" << std::endl;
			auto res = rml::roblox::internals::resolve_reflection_layout(
				code, code_address, get_string_atom_address, runtime_function_table, module_base, vft_sets, &refl_diag);
			if (res.has_value())
			{
				const auto& ev = res.value();
				std::cout << "[OK] exe=" << exe_path
						  << " capability=ReflectionLayout"
						  << " matched_calls=" << ev.matched_calls
						  << " supporting_calls=" << ev.supporting_calls
						  << " fields={"
						  << "name_offset=" << ev.name_offset
						  << ", descriptor_container_offsets=["
						  << ev.descriptor_container_offsets[0] << ","
						  << ev.descriptor_container_offsets[1] << ","
						  << ev.descriptor_container_offsets[2] << ","
						  << ev.descriptor_container_offsets[3] << ","
						  << ev.descriptor_container_offsets[4] << "]"
						  << ", base_class_offset=" << ev.base_class_offset
						  << ", functionality_offset=" << ev.functionality_offset
						  << ", owner_offset=" << ev.owner_offset
						  << ", security_offset=" << ev.security_offset
						  << ", property_type_offset=" << ev.property_type_offset
						  << ", property_functionality_offset=" << ev.property_functionality_offset
						  << ", type_tag_offset=" << ev.type_tag_offset
						  << ", type_id_offset=" << ev.type_id_offset
						  << ", type_is_float_offset=" << ev.type_is_float_offset
						  << ", type_is_number_offset=" << ev.type_is_number_offset
						  << ", type_is_enum_offset=" << ev.type_is_enum_offset
						  << ", signature_offset=" << ev.signature_offset
						  << ", function_kind_offset=" << ev.function_kind_offset
						  << ", function_invoke_func_ptr_offset=" << ev.function_invoke_func_ptr_offset
						  << ", function_bound_this_delta_offset=" << ev.function_bound_this_delta_offset
						  << ", callback_signature_offset=" << ev.callback_signature_offset
						  << ", callback_async_flag_offset=" << ev.callback_async_flag_offset
						  << "}\n";
			}
			else
			{
				append_failure(refl_diag, res.error());
			}
		}
		}

		if (has_stage(opts.selected_stages, Stage::DataModel))
		{
		// 2. DataModel Layout
		{
			std::vector<CompatibilityError> dm_diag;
			std::cout << "[RUN] exe=" << exe_path << " stage=DataModelLayout" << std::endl;
			auto res = rml::roblox::internals::resolve_datamodel_layout(
				code, code_address, runtime_function_table, module_base, datamodel_vfts, &dm_diag);
			if (res.has_value())
			{
				const auto& ev = res.value();
				std::cout << "[OK] exe=" << exe_path
						  << " capability=DataModelLayout"
						  << " matched_calls=" << ev.matched_calls
						  << " supporting_calls=" << ev.supporting_calls
						  << " fields={"
						  << "type_offset=" << ev.type_offset
						  << "}\n";
			}
			else
			{
				append_failure(dm_diag, res.error());
			}
		}
		}

		if (has_stage(opts.selected_stages, Stage::Instance))
		{
		// 3. Instance Layout
		{
			std::vector<CompatibilityError> inst_diag;
			std::cout << "[RUN] exe=" << exe_path << " stage=InstanceLayout" << std::endl;
			auto res = rml::roblox::internals::resolve_instance_layout(
				code, code_address, runtime_function_table, module_base, instance_vfts, instance_vft_entries, &inst_diag);
			if (res.has_value())
			{
				const auto& ev = res.value();
				std::cout << "[OK] exe=" << exe_path
						  << " capability=InstanceLayout"
						  << " matched_calls=" << ev.matched_calls
						  << " supporting_calls=" << ev.supporting_calls
						  << " fields={"
						  << "parent_offset=" << ev.parent_offset
						  << ", children_offset=" << ev.children_offset
						  << ", name_offset=" << ev.name_offset
						  << "}\n";
			}
			else
			{
				append_failure(inst_diag, res.error());
			}
		}
		}

		if (has_stage(opts.selected_stages, Stage::Signal))
		{
		// 4. Signal Layout
		{
			std::vector<CompatibilityError> sig_diag;
			std::cout << "[RUN] exe=" << exe_path << " stage=SignalLayout" << std::endl;
			rml::roblox::internals::SignalConnectTrace trace{};
			if (opts.has_trace_signal_connect)
			{
				trace.focus_function_address = opts.trace_signal_connect_focus;
			}

			auto res = rml::roblox::internals::resolve_signal_layout(
				code, code_address, runtime_function_table, module_base,
				signal_disconnect_address, signal_slot_free_address, &sig_diag,
				opts.has_trace_signal_connect ? &trace : nullptr);

			if (opts.has_trace_signal_connect)
			{
				std::cout << "[TRACE] focus=0x" << std::hex << trace.focus_function_address
						  << std::dec << " total_callers=" << trace.total_connect_callers
						  << " valid_candidates=" << trace.valid_connect_candidates << "\n";
				for (const auto& cand : trace.candidates)
				{
					std::cout << "[TRACE-CANDIDATE] fn=0x" << std::hex << cand.function_address
							  << " event_signal=0x" << cand.event_signal_offset
							  << " slot_source=0x" << cand.slot_source_offset
							  << " slot_wrapper_ptr=0x" << cand.slot_wrapper_ptr_offset
							  << " slot_wrapper_rep=0x" << cand.slot_wrapper_rep_offset
							  << " slot_weak=0x" << cand.slot_weak_offset
							  << " alloc_size=0x" << cand.allocation_size
							  << " insert_helper=0x" << cand.insert_helper_address
							  << " signal_head=0x" << cand.signal_head_offset
							  << " slot_strong=0x" << cand.slot_strong_offset
							  << " slot_next=0x" << cand.slot_next_offset
							  << std::dec
							  << " decoded=" << cand.decoded_instructions
							  << " event_fields=" << cand.event_field_reads
							  << " signal_addresses=" << cand.signal_address_derivations
							  << " signal_reads=" << cand.signal_object_reads
							  << " allocations=" << cand.allocation_calls
							  << " source_stores=" << cand.source_stores
							  << " wrapper_stores=" << cand.wrapper_stores
							  << " insert_calls=" << cand.insert_calls
							  << " weak_increments=" << cand.weak_increments
							  << " decode_failed=" << (cand.decode_failed ? 1 : 0)
							  << " valid=" << (cand.valid ? 1 : 0) << "\n";
				}
			}

			if (res.has_value())
			{
				const auto& ev = res.value();
				std::cout << "[OK] exe=" << exe_path
						  << " capability=SignalLayout"
						  << " matched_calls=" << ev.matched_calls
						  << " supporting_calls=" << ev.supporting_calls
						  << " fields={"
						  << "event_signal_offset=" << ev.event_signal_offset
						  << ", "
						  << "signal_head_offset=" << ev.signal_head_offset
						  << ", slot_strong_offset=" << ev.slot_strong_offset
						  << ", slot_weak_offset=" << ev.slot_weak_offset
						  << ", slot_next_offset=" << ev.slot_next_offset
						  << ", slot_source_offset=" << ev.slot_source_offset
						  << ", slot_wrapper_ptr_offset=" << ev.slot_wrapper_ptr_offset
						  << "}\n";
			}
			else
			{
				append_failure(sig_diag, res.error());
			}
		}
		}

		if (has_stage(opts.selected_stages, Stage::Job))
		{
		// 5. Job Layout
		{
			std::vector<CompatibilityError> job_diag;
			std::cout << "[RUN] exe=" << exe_path << " stage=JobLayout" << std::endl;
			std::cout << "[EVIDENCE] waiting_step=0x" << std::hex
					  << (waiting_job_step_addresses.empty() ? 0 : waiting_job_step_addresses.front())
					  << " datamodel_instance_base_offset=0x"
					  << (datamodel_instance_base_offset ? *datamodel_instance_base_offset : -1)
					  << std::dec << std::endl;
			auto res = datamodel_instance_base_offset && waiting_job_step_addresses.size() == 1
				? rml::roblox::internals::resolve_job_layout(
					code,
					code_address,
					runtime_function_table,
					module_base,
					waiting_job_vfts,
					waiting_job_step_addresses.front(),
					*datamodel_instance_base_offset,
					&job_diag)
				: std::unexpected(CompatibilityError{
					.capability = "Job.DataModelAccessor",
					.failure = CompatibilityFailure::missing_signature,
				});
			if (res.has_value())
			{
				const auto& ev = res.value();
				std::cout << "[OK] exe=" << exe_path
						  << " capability=JobLayout"
						  << " matched_calls=" << ev.matched_calls
						  << " supporting_calls=" << ev.supporting_calls
						  << " fields={"
						  << "waiting_scripts_job_script_context_offset=" << ev.waiting_scripts_job_script_context_offset
						  << ",waiting_scripts_job_data_model_accessor=0x" << std::hex
						  << ev.waiting_scripts_job_data_model_accessor << std::dec
						  << "}\n";
			}
			else
			{
				append_failure(job_diag, res.error());
			}
		}
		}

		// Deduplicate and print all diagnostics for this file
		std::vector<CompatibilityError> unique_diagnostics;
		for (const auto& diag : file_diagnostics)
		{
			bool dup = false;
			for (auto& u : unique_diagnostics)
			{
				if (u.capability == diag.capability && u.failure == diag.failure)
				{
					dup = true;
					if (diag.matched_calls > u.matched_calls || diag.decoded_candidates > u.decoded_candidates)
					{
						u = diag;
					}
					break;
				}
			}
			if (!dup)
			{
				unique_diagnostics.push_back(diag);
			}
		}

		if (!unique_diagnostics.empty())
		{
			any_failed = true;
			for (const auto& diag : unique_diagnostics)
			{
				print_failure(exe_path, diag);
			}
		}

	}

	return any_failed ? 1 : 0;
}
