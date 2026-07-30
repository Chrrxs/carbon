#include "RobloxModLoader/roblox/reflection/runtime_layout_resolver.hpp"

#include <Zydis/Zydis.h>

#include <algorithm>
#include <array>
#include <limits>
#include <optional>
#include <unordered_map>
#include <vector>

namespace rml::roblox::internals
{
	namespace
	{
		constexpr std::string_view capability_name = "Reflection.MemberTable";
		constexpr std::ptrdiff_t maximum_member_table_offset = 0x1000;

		struct DecodedInst
		{
			ZydisDecodedInstruction inst;
			std::array<ZydisDecodedOperand, ZYDIS_MAX_OPERAND_COUNT> operands;
			std::size_t offset;
		};

		bool checked_add(
			const std::uintptr_t base,
			const std::int64_t displacement,
			std::uintptr_t& result) noexcept
		{
			if (displacement >= 0)
			{
				const auto positive = static_cast<std::uint64_t>(displacement);
				if (positive > std::numeric_limits<std::uintptr_t>::max() - base)
					return false;
				result = base + static_cast<std::uintptr_t>(positive);
				return true;
			}

			const auto magnitude = std::uint64_t{0} - static_cast<std::uint64_t>(displacement);
			if (magnitude > base)
				return false;
			result = base - static_cast<std::uintptr_t>(magnitude);
			return true;
		}

		bool win_ops_has_vft_xref(
			const ZydisDecodedInstruction& inst,
			const ZydisDecodedOperand* operands,
			const std::uintptr_t inst_address,
			const std::uintptr_t vft_address) noexcept
		{
			if (inst.mnemonic != ZYDIS_MNEMONIC_LEA && inst.mnemonic != ZYDIS_MNEMONIC_MOV)
				return false;
			if (inst.operand_count < 2)
				return false;
			if (operands[0].type != ZYDIS_OPERAND_TYPE_REGISTER || operands[1].type != ZYDIS_OPERAND_TYPE_MEMORY)
				return false;
			if (operands[1].mem.base != ZYDIS_REGISTER_RIP || !operands[1].mem.disp.has_displacement)
				return false;
			std::uintptr_t target = 0;
			if (!checked_add(inst_address + inst.length, operands[1].mem.disp.value, target))
				return false;
			return target == vft_address;
		}

		bool win_ops_has_strict_vft_lea(
			const ZydisDecodedInstruction& inst,
			const ZydisDecodedOperand* operands,
			const std::uintptr_t inst_address,
			const std::span<const std::uintptr_t> vft_addresses) noexcept
		{
			if (inst.mnemonic != ZYDIS_MNEMONIC_LEA)
				return false;
			if (inst.operand_count < 2)
				return false;
			if (operands[0].type != ZYDIS_OPERAND_TYPE_REGISTER || operands[1].type != ZYDIS_OPERAND_TYPE_MEMORY)
				return false;
			if (operands[1].mem.base != ZYDIS_REGISTER_RIP || !operands[1].mem.disp.has_displacement)
				return false;
			std::uintptr_t target = 0;
			if (!checked_add(inst_address + inst.length, operands[1].mem.disp.value, target))
				return false;
			for (const auto vft : vft_addresses)
			{
				if (target == vft)
					return true;
			}
			return false;
		}
#pragma pack(push, 1)
		struct RuntimeFunctionEntry
		{
			std::uint32_t begin_address;
			std::uint32_t end_address;
			std::uint32_t unwind_info_address;
		};
#pragma pack(pop)

		static_assert(sizeof(RuntimeFunctionEntry) == 12);

		struct FunctionBounds
		{
			std::size_t begin;
			std::size_t end;

			constexpr auto operator<=>(const FunctionBounds&) const noexcept = default;
		};

		std::expected<FunctionBounds, CompatibilityFailure> find_function_bounds(
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			const std::span<const std::byte> runtime_function_table,
			const std::uintptr_t module_address,
			const std::size_t xref_offset) noexcept
		{
			if (runtime_function_table.empty() ||
				runtime_function_table.size() % sizeof(RuntimeFunctionEntry) != 0 ||
				code_address < module_address ||
				xref_offset >= code.size())
			{
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			}

			const auto xref_address = code_address + xref_offset;
			if (xref_address < module_address ||
				xref_address - module_address > std::numeric_limits<std::uint32_t>::max())
			{
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			}
			const auto xref_rva = static_cast<std::uint32_t>(xref_address - module_address);

			const auto entries = std::span(
				reinterpret_cast<const RuntimeFunctionEntry*>(runtime_function_table.data()),
				runtime_function_table.size() / sizeof(RuntimeFunctionEntry));
			std::size_t first = 0;
			std::size_t last = entries.size();
			while (first < last)
			{
				const auto middle = first + (last - first) / 2;
				if (entries[middle].begin_address <= xref_rva)
					first = middle + 1;
				else
					last = middle;
			}
			if (first == 0)
				return std::unexpected(CompatibilityFailure::missing_signature);

			const auto& entry = entries[first - 1];
			if (entry.begin_address == 0 && entry.end_address == 0 && entry.unwind_info_address == 0)
				return std::unexpected(CompatibilityFailure::missing_signature);
			if (entry.begin_address >= entry.end_address)
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			if (xref_rva < entry.begin_address || xref_rva >= entry.end_address)
				return std::unexpected(CompatibilityFailure::missing_signature);

			std::uintptr_t function_begin = 0;
			std::uintptr_t function_end = 0;
			if (!checked_add(module_address, entry.begin_address, function_begin) ||
				!checked_add(module_address, entry.end_address, function_end) ||
				function_begin < code_address ||
				function_end > code_address + code.size() ||
				function_begin >= function_end)
			{
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			}
			return FunctionBounds{
				.begin = static_cast<std::size_t>(function_begin - code_address),
				.end = static_cast<std::size_t>(function_end - code_address),
			};
		}

		struct PatternByte
		{
			bool is_wildcard{true};
			std::uint8_t value{0};
		};

		std::uintptr_t pattern_scan_signature(
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			const std::span<const PatternByte> pattern,
			std::size_t* match_count = nullptr) noexcept
		{
			if (code.size() < pattern.size())
				return 0;

			std::uintptr_t matched_addr = 0;
			std::size_t matches = 0;

			for (std::size_t i = 0; i <= code.size() - pattern.size(); ++i)
			{
				bool match = true;
				for (std::size_t j = 0; j < pattern.size(); ++j)
				{
					if (!pattern[j].is_wildcard &&
						static_cast<std::uint8_t>(code[i + j]) != pattern[j].value)
					{
						match = false;
						break;
					}
				}
				if (match)
				{
					++matches;
					matched_addr = code_address + i;
				}
			}
			if (match_count)
				*match_count = matches;
			return matches == 1 ? matched_addr : 0;
		}

		std::uintptr_t pattern_scan_signal_disconnect(
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			std::size_t* match_count = nullptr) noexcept
		{
			constexpr PatternByte pattern[] = {
				{false, 0x48}, {false, 0x89}, {false, 0x5C}, {false, 0x24}, {true, 0x00},
				{false, 0x57}, {false, 0x48}, {false, 0x83}, {false, 0xEC}, {false, 0x30},
				{false, 0x48}, {false, 0x8B}, {false, 0xF9}, {false, 0x33}, {false, 0xDB},
				{false, 0x48}, {false, 0x89}, {false, 0x5C}, {false, 0x24}, {true, 0x00},
				{false, 0xE8}, {true, 0x00},  {true, 0x00},  {true, 0x00},  {true, 0x00},
				{false, 0x48}, {false, 0x89}, {false, 0x44}, {false, 0x24}, {true, 0x00},
				{false, 0x88}, {false, 0x5C}, {false, 0x24}, {true, 0x00}, {false, 0x48},
				{false, 0x8B}, {false, 0xC8}, {false, 0xE8}, {true, 0x00},  {true, 0x00},
				{true, 0x00},  {true, 0x00},  {false, 0x85}, {false, 0xC0}, {false, 0x0F},
				{false, 0x85}
			};
			return pattern_scan_signature(code, code_address, pattern, match_count);
		}

		std::uintptr_t pattern_scan_signal_slot_free(
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			std::size_t* match_count = nullptr) noexcept
		{
			constexpr PatternByte pattern[] = {
				{false, 0x48}, {false, 0x89}, {false, 0x5C}, {false, 0x24}, {false, 0x10},
				{false, 0x48}, {false, 0x89}, {false, 0x74}, {false, 0x24}, {false, 0x18},
				{false, 0x57}, {false, 0x48}, {false, 0x83}, {false, 0xEC}, {false, 0x20},
				{false, 0x48}, {false, 0x8B}, {false, 0xD9}, {false, 0xE8}, {true, 0x00},
				{true, 0x00},  {true, 0x00},  {true, 0x00},  {false, 0x48}, {false, 0x8D},
				{false, 0x50}, {false, 0x10}, {false, 0xBF}, {false, 0xFF}, {false, 0xFF},
				{false, 0xFF}, {false, 0xFF}, {false, 0x48}, {false, 0x3B}, {false, 0xDA},
				{false, 0x0F}, {false, 0x82}
			};
			return pattern_scan_signature(code, code_address, pattern, match_count);
		}

		std::uintptr_t pattern_scan_signal_slot_insert(
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			std::size_t* match_count = nullptr) noexcept
		{
			constexpr PatternByte pattern[] = {
				{false, 0x40}, {false, 0x53}, {false, 0x56}, {false, 0x57},
				{false, 0x48}, {false, 0x83}, {false, 0xEC}, {false, 0x30},
				{false, 0x48}, {false, 0x8B}, {false, 0xF2}, {false, 0x48},
				{false, 0x8B}, {false, 0xF9}, {false, 0x33}, {false, 0xDB},
				{false, 0x89}, {false, 0x5C}, {false, 0x24},
			};
			return pattern_scan_signature(code, code_address, pattern, match_count);
		}

		ZydisRegister to_gpr32(ZydisRegister reg) noexcept
		{
			if (reg == ZYDIS_REGISTER_NONE)
				return ZYDIS_REGISTER_NONE;
			return ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, reg);
		}

		enum class RegRole
		{
			Unknown,
			This,
			Arg1,
			Arg2,
			Arg3,
			WrapperPtr,
			WrapperRep,
			SlotField,
			SignalField,
			EventSignalField,
			SignalAddress,
			SignalObject,
			Zero,
			VftAddr,
			AllocatedSlot,
			SignalPtr,
			SlotPtr,
			CodePtr,
			ImmVal,
			Clobbered,
		};

		struct RegisterState
		{
			RegRole role{RegRole::Unknown};
			std::uint64_t imm_value{0};
		};

		struct RegTracker
		{
			std::array<RegisterState, ZYDIS_REGISTER_MAX_VALUE + 1> regs{};
			std::vector<std::pair<std::ptrdiff_t, RegRole>> stack_spills{};
			std::size_t pending_alloc_size{0};
			RegRole initial_rcx{RegRole::Unknown};

			RegTracker() noexcept
			{
				set_role(ZYDIS_REGISTER_RCX, RegRole::This);
				set_role(ZYDIS_REGISTER_RDX, RegRole::Arg1);
				set_role(ZYDIS_REGISTER_R8, RegRole::Arg2);
				set_role(ZYDIS_REGISTER_R9, RegRole::Arg3);
			}

			RegRole get_role(ZydisRegister reg) const noexcept
			{
				ZydisRegister canonical = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, reg);
				if (canonical == ZYDIS_REGISTER_NONE)
					return RegRole::Unknown;
				return regs[canonical].role;
			}

			std::uint64_t get_imm(ZydisRegister reg) const noexcept
			{
				ZydisRegister canonical = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, reg);
				if (canonical == ZYDIS_REGISTER_NONE)
					return 0;
				return regs[canonical].imm_value;
			}

			void set_role(ZydisRegister reg, RegRole role, std::uint64_t imm = 0) noexcept
			{
				ZydisRegister canonical = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, reg);
				if (canonical != ZYDIS_REGISTER_NONE)
				{
					regs[canonical].role = role;
					regs[canonical].imm_value = imm;
				}
			}

			RegRole get_stack_role(std::ptrdiff_t disp) const noexcept
			{
				for (const auto& [d, r] : stack_spills)
				{
					if (d == disp)
						return r;
				}
				return RegRole::Unknown;
			}

			void set_stack_role(std::ptrdiff_t disp, RegRole role) noexcept
			{
				for (auto& [d, r] : stack_spills)
				{
					if (d == disp)
					{
						r = role;
						return;
					}
				}
				stack_spills.push_back({disp, role});
			}

			void handle_call() noexcept
			{
				static constexpr ZydisRegister volatile_regs[] = {
					ZYDIS_REGISTER_RAX, ZYDIS_REGISTER_RCX, ZYDIS_REGISTER_RDX,
					ZYDIS_REGISTER_R8,  ZYDIS_REGISTER_R9,  ZYDIS_REGISTER_R10,
					ZYDIS_REGISTER_R11
				};
				for (const auto vreg : volatile_regs)
				{
					set_role(vreg, RegRole::Clobbered);
				}
			}

			void update(
				const ZydisDecodedInstruction& inst,
				const ZydisDecodedOperand* operands,
				std::uintptr_t inst_address) noexcept
			{
				if (inst.mnemonic == ZYDIS_MNEMONIC_CALL)
				{
					handle_call();
					return;
				}

				if (inst.mnemonic == ZYDIS_MNEMONIC_MOV && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY && operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
				{
					ZydisRegister mem_base = to_gpr32(operands[0].mem.base);
					if (mem_base == ZYDIS_REGISTER_RSP || mem_base == ZYDIS_REGISTER_RBP)
					{
						const auto src_role = get_role(operands[1].reg.value);
						if (src_role != RegRole::Unknown && src_role != RegRole::Clobbered && operands[0].mem.disp.has_displacement)
						{
							set_stack_role(static_cast<std::ptrdiff_t>(operands[0].mem.disp.value), src_role);
						}
					}
				}

				if (inst.operand_count >= 1 && operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER)
				{
					ZydisRegister dest = operands[0].reg.value;
					if (dest == ZYDIS_REGISTER_NONE)
						return;

					if (inst.mnemonic == ZYDIS_MNEMONIC_MOV || inst.mnemonic == ZYDIS_MNEMONIC_MOVZX || inst.mnemonic == ZYDIS_MNEMONIC_MOVSX)
					{
						if (inst.operand_count >= 2)
						{
							if (operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
							{
								set_role(dest, get_role(operands[1].reg.value), get_imm(operands[1].reg.value));
							}
							else if (operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE)
							{
								std::uint64_t val = operands[1].imm.value.u;
								set_role(dest, val == 0 ? RegRole::Zero : RegRole::ImmVal, val);
							}
							else if (operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
							{
								ZydisRegister mem_base = to_gpr32(operands[1].mem.base);
								if ((mem_base == ZYDIS_REGISTER_RSP || mem_base == ZYDIS_REGISTER_RBP) && operands[1].mem.disp.has_displacement)
								{
									const auto srole = get_stack_role(static_cast<std::ptrdiff_t>(operands[1].mem.disp.value));
									if (srole != RegRole::Unknown)
										set_role(dest, srole);
									else
										set_role(dest, RegRole::Clobbered);
								}
								else
								{
									const auto base_role = get_role(operands[1].mem.base);
									const auto disp = operands[1].mem.disp.has_displacement
										? static_cast<std::ptrdiff_t>(operands[1].mem.disp.value) : 0;
									if (base_role == RegRole::Arg3 && disp == 0)
										set_role(dest, RegRole::WrapperPtr);
									else if (base_role == RegRole::Arg3 &&
										disp == static_cast<std::ptrdiff_t>(sizeof(void*)))
										set_role(dest, RegRole::WrapperRep);
									else if (base_role == RegRole::AllocatedSlot || base_role == RegRole::SlotPtr)
										set_role(dest, RegRole::SlotField, static_cast<std::uint64_t>(disp));
									else if (base_role == RegRole::SignalPtr)
										set_role(dest, RegRole::SignalField, static_cast<std::uint64_t>(disp));
									else if (base_role == RegRole::This)
										set_role(dest, RegRole::SignalPtr);
									else if (base_role == RegRole::Arg1 || base_role == RegRole::Arg2 ||
										base_role == RegRole::Arg3)
										set_role(dest, base_role);
									else
										set_role(dest, RegRole::Clobbered);
								}
							}
						}
					}
					else if (inst.mnemonic == ZYDIS_MNEMONIC_XOR || inst.mnemonic == ZYDIS_MNEMONIC_SUB)
					{
						if (inst.operand_count >= 2 && operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							to_gpr32(operands[0].reg.value) == to_gpr32(operands[1].reg.value))
						{
							set_role(dest, RegRole::Zero, 0);
						}
						else
						{
							set_role(dest, RegRole::Clobbered);
						}
					}
					else if (inst.mnemonic == ZYDIS_MNEMONIC_LEA)
					{
						if (inst.operand_count >= 2 && operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
						{
							if (operands[1].mem.base == ZYDIS_REGISTER_RIP)
							{
								if (get_role(dest) != RegRole::VftAddr)
									set_role(dest, RegRole::CodePtr);
							}
							else
							{
								const auto base_r = get_role(operands[1].mem.base);
								if (base_r == RegRole::This || base_r == RegRole::SignalPtr)
									set_role(dest, RegRole::SignalPtr);
								else if (base_r == RegRole::AllocatedSlot || base_r == RegRole::SlotPtr)
									set_role(dest, RegRole::SlotPtr);
								else
									set_role(dest, RegRole::Clobbered);
							}
						}
					}
					else
					{
						if (operands[0].actions & ZYDIS_OPERAND_ACTION_MASK_WRITE)
						{
							set_role(dest, RegRole::Clobbered);
						}
					}
				}
			}
		};
	}

	std::expected<ReflectionLayoutEvidence, CompatibilityError> resolve_reflection_layout(
		const std::span<const std::byte> executable_code,
		const std::uintptr_t code_address,
		const std::uintptr_t get_string_atom_address,
		const std::span<const std::byte> runtime_function_table,
		const std::uintptr_t module_address,
		const ReflectionVftSets& vft_sets,
		std::vector<CompatibilityError>* diagnostics) noexcept
	{
		auto emit = [diagnostics](CompatibilityError err) {
			if (diagnostics)
				diagnostics->push_back(err);
			return err;
		};
		if (executable_code.empty() || code_address > std::numeric_limits<std::uintptr_t>::max() - executable_code.size())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::invalid_address_range,
			}));
		}

		if (runtime_function_table.empty() || module_address == 0 || get_string_atom_address == 0)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::invalid_address_range,
			}));
		}

		const bool class_only =
			vft_sets.descriptor_vfts.empty() &&
			vft_sets.member_vfts.empty() &&
			vft_sets.property_vfts.empty() &&
			vft_sets.function_vfts.empty() &&
			vft_sets.yield_function_vfts.empty() &&
			vft_sets.event_vfts.empty() &&
			vft_sets.callback_vfts.empty() &&
			!vft_sets.class_descriptor_vfts.empty();
		if ((!class_only && (vft_sets.descriptor_vfts.empty() ||
			vft_sets.member_vfts.empty() ||
			vft_sets.property_vfts.empty() ||
			vft_sets.function_vfts.empty() ||
			vft_sets.yield_function_vfts.empty() ||
			vft_sets.event_vfts.empty() ||
			vft_sets.callback_vfts.empty())) ||
			vft_sets.class_descriptor_vfts.empty())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::missing_signature,
			}));
		}

		ZydisDecoder decoder;
		if (!ZYAN_SUCCESS(ZydisDecoderInit(&decoder, ZYDIS_MACHINE_MODE_LONG_64, ZYDIS_STACK_WIDTH_64)))
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::unsupported_instruction_form,
			}));
		}

		struct DerivedField
		{
			std::ptrdiff_t val{-1};
			bool has_conflict{false};

			void add_candidate(std::ptrdiff_t offset) noexcept
			{
				if (offset <= 0) return;
				if (val == -1)
				{
					val = offset;
				}
				else if (val != offset)
				{
					has_conflict = true;
				}
			}
		};

		DerivedField field_name;
		DerivedField field_owner;
		DerivedField field_security;
		DerivedField field_property_type;
		DerivedField field_property_func;
		DerivedField field_signature;
		DerivedField field_func_kind;
		DerivedField field_func_invoke;
		DerivedField field_func_this_delta;
		DerivedField field_callback_sig;
		DerivedField field_callback_async;
		DerivedField field_event_signal;

		std::optional<std::array<std::ptrdiff_t, 5>> class_containers;
		DerivedField field_base_class;
		DerivedField field_class_func;
		std::size_t class_decoded_candidates = 0;

		std::size_t matched_calls = 0;
		std::vector<std::pair<std::size_t, std::size_t>> processed_bounds;
		enum VftFamily : std::uint16_t
		{
			descriptor_family = 1u << 0,
			member_family = 1u << 1,
			property_family = 1u << 2,
			function_family = 1u << 3,
			yield_family = 1u << 4,
			event_family = 1u << 5,
			callback_family = 1u << 6,
			class_family = 1u << 7,
		};
		std::unordered_map<std::uintptr_t, std::uint16_t> vft_families;
		const auto vft_count =
			vft_sets.descriptor_vfts.size() +
			vft_sets.member_vfts.size() +
			vft_sets.property_vfts.size() +
			vft_sets.function_vfts.size() +
			vft_sets.yield_function_vfts.size() +
			vft_sets.event_vfts.size() +
			vft_sets.callback_vfts.size() +
			vft_sets.class_descriptor_vfts.size();
		vft_families.reserve(vft_count);
		const auto add_family = [&vft_families](const std::span<const std::uintptr_t> addresses, const VftFamily family) {
			for (const auto address : addresses)
				vft_families[address] |= family;
		};
		add_family(vft_sets.descriptor_vfts, descriptor_family);
		add_family(vft_sets.member_vfts, member_family);
		add_family(vft_sets.property_vfts, property_family);
		add_family(vft_sets.function_vfts, function_family);
		add_family(vft_sets.yield_function_vfts, yield_family);
		add_family(vft_sets.event_vfts, event_family);
		add_family(vft_sets.callback_vfts, callback_family);
		add_family(vft_sets.class_descriptor_vfts, class_family);

		const auto* code_bytes = reinterpret_cast<const std::uint8_t*>(executable_code.data());
		std::size_t cursor = 0;
		while (cursor + 7 <= executable_code.size())
		{
			const auto* opcode = static_cast<const std::uint8_t*>(
				std::memchr(code_bytes + cursor + 1, 0x8D, executable_code.size() - cursor - 1));
			if (opcode == nullptr)
				break;
			const auto opcode_offset = static_cast<std::size_t>(opcode - code_bytes);
			if (executable_code.size() - opcode_offset < 6)
				break;
			cursor = opcode_offset - 1;
			const auto* bytes = code_bytes + cursor;
			if ((bytes[0] & 0xF8) != 0x48 || (bytes[2] & 0xC7) != 0x05)
			{
				cursor += 2;
				continue;
			}
			std::int32_t displacement = 0;
			std::memcpy(&displacement, bytes + 3, sizeof(displacement));
			std::uintptr_t target = 0;
			if (!checked_add(code_address + cursor + 7, displacement, target) ||
				!vft_families.contains(target))
			{
				++cursor;
				continue;
			}
			ZydisDecodedInstruction inst;
			ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];

			if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(&decoder, bytes, executable_code.size() - cursor, &inst, operands)) || inst.length == 0)
			{
				cursor += 1;
				continue;
			}

			std::uint16_t families = 0;
			if (inst.mnemonic == ZYDIS_MNEMONIC_LEA &&
				inst.operand_count >= 2 &&
				operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
				operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
				operands[1].mem.base == ZYDIS_REGISTER_RIP &&
				operands[1].mem.disp.has_displacement)
			{
				std::uintptr_t target = 0;
				if (checked_add(code_address + cursor + inst.length, operands[1].mem.disp.value, target))
				{
					if (const auto family = vft_families.find(target); family != vft_families.end())
						families = family->second;
				}
			}
			const bool is_descriptor_vft = (families & descriptor_family) != 0;
			const bool is_member_vft = (families & member_family) != 0;
			const bool is_property_vft = (families & property_family) != 0;
			const bool is_function_vft = (families & function_family) != 0;
			const bool is_yield_vft = (families & yield_family) != 0;
			const bool is_event_vft = (families & event_family) != 0;
			const bool is_callback_vft = (families & callback_family) != 0;
			const bool is_class_vft = (families & class_family) != 0;

			if (!is_descriptor_vft && !is_member_vft && !is_property_vft && !is_function_vft &&
				!is_yield_vft && !is_event_vft && !is_callback_vft && !is_class_vft)
			{
				cursor += inst.length;
				continue;
			}

			const auto vft_reg = operands[0].reg.value;
			++matched_calls;

			auto bounds_result = find_function_bounds(
				executable_code, code_address, runtime_function_table, module_address, cursor);
			std::optional<FunctionBounds> bounds;
			if (bounds_result)
			{
				bounds = *bounds_result;
			}
			else if (bounds_result.error() == CompatibilityFailure::missing_signature)
			{
				// Leaf constructors have no RUNTIME_FUNCTION entry. The strict family
				// VFT reference is the semantic anchor; inspect only its straight-line tail.
				constexpr std::size_t maximum_leaf_tail = 128;
				const auto limit = std::min(executable_code.size(), cursor + maximum_leaf_tail);
				std::size_t leaf_pos = cursor;
				while (leaf_pos < limit)
				{
					const auto* leaf_bytes =
						reinterpret_cast<const std::uint8_t*>(executable_code.data() + leaf_pos);
					ZydisDecodedInstruction leaf_inst{};
					ZydisDecodedOperand leaf_operands[ZYDIS_MAX_OPERAND_COUNT]{};
					if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
							&decoder, leaf_bytes, limit - leaf_pos, &leaf_inst, leaf_operands)) ||
						leaf_inst.length == 0 || leaf_inst.mnemonic == ZYDIS_MNEMONIC_INT3 ||
						leaf_inst.meta.category == ZYDIS_CATEGORY_CALL ||
						leaf_inst.meta.category == ZYDIS_CATEGORY_COND_BR ||
						leaf_inst.meta.category == ZYDIS_CATEGORY_UNCOND_BR)
					{
						break;
					}
					leaf_pos += leaf_inst.length;
					if (leaf_inst.mnemonic == ZYDIS_MNEMONIC_RET)
					{
						bounds = FunctionBounds{.begin = cursor, .end = leaf_pos};
						break;
					}
				}
			}
			if (!bounds)
			{
				cursor += inst.length;
				continue;
			}

			const std::pair<std::size_t, std::size_t> fn_range{bounds->begin, bounds->end};
			if (std::find(processed_bounds.begin(), processed_bounds.end(), fn_range) != processed_bounds.end())
			{
				cursor += inst.length;
				continue;
			}
			processed_bounds.push_back(fn_range);

			std::vector<DecodedInst> func_instructions;
			std::size_t scan_pos = bounds->begin;
			while (scan_pos < bounds->end)
			{
				const auto* scan_bytes = reinterpret_cast<const std::uint8_t*>(executable_code.data() + scan_pos);
				DecodedInst dinst{};
				dinst.offset = scan_pos;

				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(&decoder, scan_bytes, bounds->end - scan_pos, &dinst.inst, dinst.operands.data())) || dinst.inst.length == 0)
				{
					scan_pos += 1;
					continue;
				}

				func_instructions.push_back(dinst);
				scan_pos += dinst.inst.length;
			}

			ZydisRegister this_reg = ZYDIS_REGISTER_NONE;
			for (const auto& dinst : func_instructions)
			{
				if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV && dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER && dinst.operands[1].reg.value == vft_reg &&
					(!dinst.operands[0].mem.disp.has_displacement || dinst.operands[0].mem.disp.value == 0))
				{
					this_reg = dinst.operands[0].mem.base;
					break;
				}
			}
			if (this_reg == ZYDIS_REGISTER_NONE)
			{
				this_reg = ZYDIS_REGISTER_RCX;
			}

			struct ZeroStoreRun
			{
				std::vector<std::ptrdiff_t> qword_offsets;
				std::ptrdiff_t trailing_scalar_offset{-1};
				std::uint16_t trailing_scalar_size{0};
			};
			std::vector<ZeroStoreRun> zero_store_runs;
			for (std::size_t i = 0; i < func_instructions.size(); ++i)
			{
				const auto& zero = func_instructions[i];
				if ((zero.inst.mnemonic != ZYDIS_MNEMONIC_XOR &&
					 zero.inst.mnemonic != ZYDIS_MNEMONIC_SUB) ||
					zero.operands[0].type != ZYDIS_OPERAND_TYPE_REGISTER ||
					zero.operands[1].type != ZYDIS_OPERAND_TYPE_REGISTER ||
					to_gpr32(zero.operands[0].reg.value) != to_gpr32(zero.operands[1].reg.value))
				{
					continue;
				}

				const auto zero_register = to_gpr32(zero.operands[0].reg.value);
				ZeroStoreRun run;
				for (std::size_t j = i + 1; j < func_instructions.size(); ++j)
				{
					const auto& store = func_instructions[j];
					if (store.inst.mnemonic != ZYDIS_MNEMONIC_MOV ||
						store.operands[0].type != ZYDIS_OPERAND_TYPE_MEMORY ||
						store.operands[0].mem.base != this_reg ||
						!store.operands[0].mem.disp.has_displacement ||
						store.operands[1].type != ZYDIS_OPERAND_TYPE_REGISTER ||
						to_gpr32(store.operands[1].reg.value) != zero_register)
					{
						break;
					}

					const auto disp = static_cast<std::ptrdiff_t>(store.operands[0].mem.disp.value);
					if (store.operands[0].size == 64 &&
						(run.qword_offsets.empty() ||
						 disp == run.qword_offsets.back() + static_cast<std::ptrdiff_t>(sizeof(std::uintptr_t))))
					{
						run.qword_offsets.push_back(disp);
						continue;
					}
					if (!run.qword_offsets.empty() &&
						disp == run.qword_offsets.back() + static_cast<std::ptrdiff_t>(sizeof(std::uintptr_t)) &&
						(store.operands[0].size == 8 || store.operands[0].size == 32))
					{
						run.trailing_scalar_offset = disp;
						run.trailing_scalar_size = store.operands[0].size;
					}
					break;
				}
				if (!run.qword_offsets.empty())
					zero_store_runs.push_back(std::move(run));
			}

			// 1. Descriptor name_offset derivation:
			if (is_descriptor_vft)
			{
				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_CALL)
					{
						bool calls_get_string_atom = false;
						const auto inst_addr = code_address + dinst.offset;
						if (dinst.operands[0].type == ZYDIS_OPERAND_TYPE_IMMEDIATE)
						{
							std::uintptr_t target = 0;
							if (checked_add(inst_addr + dinst.inst.length, dinst.operands[0].imm.value.s, target) && target == get_string_atom_address)
								calls_get_string_atom = true;
						}
						else if (dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY && dinst.operands[0].mem.base == ZYDIS_REGISTER_RIP)
						{
							std::uintptr_t target_ptr = 0;
							if (checked_add(inst_addr + dinst.inst.length, dinst.operands[0].mem.disp.value, target_ptr))
							{
								if (target_ptr == get_string_atom_address)
								{
									calls_get_string_atom = true;
								}
								else if (target_ptr >= code_address && target_ptr + sizeof(std::uintptr_t) <= code_address + executable_code.size())
								{
									const auto offset_in_code = target_ptr - code_address;
									std::uintptr_t deref = 0;
									std::memcpy(&deref, executable_code.data() + offset_in_code, sizeof(deref));
									if (deref == get_string_atom_address)
										calls_get_string_atom = true;
								}
							}
						}

						if (calls_get_string_atom)
						{
							const std::size_t lookahead = std::min(func_instructions.size(), i + 16);
							for (std::size_t j = i + 1; j < lookahead; ++j)
							{
								const auto& sub = func_instructions[j];
								if (sub.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
									sub.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
									sub.operands[0].mem.base == this_reg &&
									sub.operands[0].mem.disp.has_displacement &&
									sub.operands[0].mem.disp.value > 0 &&
									sub.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
									to_gpr32(sub.operands[1].reg.value) == ZYDIS_REGISTER_RAX)
								{
									field_name.add_candidate(static_cast<std::ptrdiff_t>(sub.operands[0].mem.disp.value));
									break;
								}
							}
						}
					}
				}

				for (const auto& dinst : func_instructions)
				{
					if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[0].mem.base == this_reg &&
						dinst.operands[0].mem.disp.has_displacement &&
						dinst.operands[0].mem.disp.value > 0 &&
						dinst.operands[0].size == 64 &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						to_gpr32(dinst.operands[1].reg.value) == ZYDIS_REGISTER_RDX)
					{
						field_name.add_candidate(static_cast<std::ptrdiff_t>(dinst.operands[0].mem.disp.value));
					}
				}
			}

			// 2. MemberDescriptor fields (owner_offset, security_offset):
			if (is_member_vft)
			{
				RegTracker tracker;
				std::ptrdiff_t owner_candidate = -1;
				for (const auto& dinst : func_instructions)
				{
					if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_LEA &&
						win_ops_has_strict_vft_lea(
							dinst.inst, dinst.operands.data(),
							code_address + dinst.offset, vft_sets.member_vfts))
					{
						tracker.set_role(dinst.operands[0].reg.value, RegRole::VftAddr);
					}
					else if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						tracker.get_role(dinst.operands[1].reg.value) == RegRole::VftAddr)
					{
						tracker.set_role(dinst.operands[0].mem.base, RegRole::This);
					}
					else if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[0].mem.disp.has_displacement &&
						tracker.get_role(dinst.operands[0].mem.base) == RegRole::This)
					{
						const auto disp =
							static_cast<std::ptrdiff_t>(dinst.operands[0].mem.disp.value);
						if (dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							tracker.get_role(dinst.operands[1].reg.value) == RegRole::Arg1)
						{
							field_owner.add_candidate(disp);
							owner_candidate = disp;
						}
						else if (owner_candidate > 0 && disp == owner_candidate + 8)
						{
							field_security.add_candidate(disp);
						}
					}
					tracker.update(dinst.inst, dinst.operands.data(), code_address + dinst.offset);
				}
			}

			// 3. PropertyDescriptor fields (property_type_offset, property_functionality_offset):
			if (is_property_vft)
			{
				RegTracker tracker;
				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					if (dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[0].mem.base == this_reg &&
						dinst.operands[0].mem.disp.has_displacement)
					{
						const auto disp = static_cast<std::ptrdiff_t>(dinst.operands[0].mem.disp.value);
						if (disp > 0)
						{
							if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
								dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								tracker.get_role(dinst.operands[1].reg.value) == RegRole::Arg2)
							{
								field_property_type.add_candidate(disp);
							}
							else if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_XOR &&
									 dinst.operands[0].size == 32 &&
									 dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
							{
								field_property_func.add_candidate(disp);
							}
							else if ((dinst.inst.mnemonic == ZYDIS_MNEMONIC_OR || dinst.inst.mnemonic == ZYDIS_MNEMONIC_AND ||
									  dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV || dinst.inst.mnemonic == ZYDIS_MNEMONIC_TEST ||
									  dinst.inst.mnemonic == ZYDIS_MNEMONIC_CMP) &&
									 dinst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
									 (dinst.operands[1].imm.value.u & 0x3) != 0)
							{
								field_property_func.add_candidate(disp);
							}
						}
					}
					tracker.update(dinst.inst, dinst.operands.data(), code_address + dinst.offset);
				}
			}

			// 4. FunctionDescriptor & YieldFunctionDescriptor fields:
			if (is_function_vft || is_yield_vft)
			{
				// Signature is a three-word in-place object, followed by the three-word
				// callable state and a scalar FunctionKind. Derive the member starts from
				// the constructor's contiguous zero-initialization run.
				for (const auto& run : zero_store_runs)
				{
					if (run.qword_offsets.size() < 6 || run.trailing_scalar_size != 32)
						continue;
					field_signature.add_candidate(run.qword_offsets[0]);
					field_func_invoke.add_candidate(run.qword_offsets[3]);
					field_func_this_delta.add_candidate(run.qword_offsets[4]);
					field_func_kind.add_candidate(run.trailing_scalar_offset);
				}
				std::size_t vft_store_index = func_instructions.size();
				ZydisRegister vft_register = ZYDIS_REGISTER_NONE;
				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					const auto matches_family_vft =
						win_ops_has_strict_vft_lea(
							dinst.inst, dinst.operands.data(), code_address + dinst.offset,
							vft_sets.function_vfts) ||
						win_ops_has_strict_vft_lea(
							dinst.inst, dinst.operands.data(), code_address + dinst.offset,
							vft_sets.yield_function_vfts);
					if (matches_family_vft)
					{
						vft_register = dinst.operands[0].reg.value;
						continue;
					}
					if (vft_register != ZYDIS_REGISTER_NONE &&
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						dinst.operands[1].reg.value == vft_register)
					{
						vft_store_index = i;
						break;
					}
				}
				std::vector<std::ptrdiff_t> constructed_signatures;

				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[0].mem.base == this_reg &&
						dinst.operands[0].mem.disp.has_displacement)
					{
						const auto disp = static_cast<std::ptrdiff_t>(dinst.operands[0].mem.disp.value);
						if (disp > 0 && dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							to_gpr32(dinst.operands[1].reg.value) == ZYDIS_REGISTER_R9)
						{
							field_signature.add_candidate(disp);
						}
						if (disp > 0 && dinst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
							dinst.operands[1].imm.value.u > 0 && dinst.operands[1].imm.value.u < 0x10 &&
							(dinst.operands[0].size == 8 || dinst.operands[0].size == 32))
						{
							field_func_kind.add_candidate(disp);
						}
					}

					if (i > vft_store_index && dinst.inst.mnemonic == ZYDIS_MNEMONIC_LEA &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].mem.base == this_reg &&
						dinst.operands[1].mem.disp.has_displacement)
					{
						const auto disp = static_cast<std::ptrdiff_t>(dinst.operands[1].mem.disp.value);
						const auto lookahead = std::min(func_instructions.size(), i + 5);
						for (std::size_t j = i + 1; j < lookahead; ++j)
						{
							if (func_instructions[j].inst.mnemonic == ZYDIS_MNEMONIC_CALL)
							{
								if (disp > 0)
									constructed_signatures.push_back(disp);
								break;
							}
						}
					}

					if (i > vft_store_index && dinst.inst.mnemonic == ZYDIS_MNEMONIC_LEA &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].mem.base == ZYDIS_REGISTER_RIP)
					{
						const auto code_register = dinst.operands[0].reg.value;
						const auto store_limit = std::min(func_instructions.size(), i + 8);
						for (std::size_t j = i + 1; j < store_limit; ++j)
						{
							const auto& store = func_instructions[j];
							if (store.inst.meta.category == ZYDIS_CATEGORY_CALL ||
								store.inst.meta.category == ZYDIS_CATEGORY_COND_BR ||
								store.inst.meta.category == ZYDIS_CATEGORY_UNCOND_BR)
							{
								break;
							}
							if (store.inst.mnemonic != ZYDIS_MNEMONIC_MOV ||
								store.operands[0].type != ZYDIS_OPERAND_TYPE_MEMORY ||
								store.operands[0].mem.base != this_reg ||
								!store.operands[0].mem.disp.has_displacement ||
								store.operands[1].type != ZYDIS_OPERAND_TYPE_REGISTER ||
								store.operands[1].reg.value != code_register)
							{
								continue;
							}

							const auto invoke_disp =
								static_cast<std::ptrdiff_t>(store.operands[0].mem.disp.value);
							const auto delta_limit = std::min(func_instructions.size(), j + 6);
							for (std::size_t k = j + 1; k < delta_limit; ++k)
							{
								const auto& delta = func_instructions[k];
								if (delta.inst.meta.category == ZYDIS_CATEGORY_CALL ||
									delta.inst.meta.category == ZYDIS_CATEGORY_COND_BR ||
									delta.inst.meta.category == ZYDIS_CATEGORY_UNCOND_BR)
								{
									break;
								}
								if (delta.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
									delta.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
									delta.operands[0].mem.base == this_reg &&
									delta.operands[0].mem.disp.has_displacement &&
									delta.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
									delta.operands[1].imm.value.u == 0)
								{
									const auto delta_disp =
										static_cast<std::ptrdiff_t>(delta.operands[0].mem.disp.value);
									if (invoke_disp > 0 && delta_disp > invoke_disp &&
										delta_disp - invoke_disp == static_cast<std::ptrdiff_t>(sizeof(std::uintptr_t)))
									{
										field_func_invoke.add_candidate(invoke_disp);
										field_func_this_delta.add_candidate(delta_disp);
									}
									break;
								}
							}
							break;
						}
					}
				}

				std::sort(constructed_signatures.begin(), constructed_signatures.end());
				constructed_signatures.erase(
					std::unique(constructed_signatures.begin(), constructed_signatures.end()),
					constructed_signatures.end());
				if (field_signature.val == -1 && constructed_signatures.size() == 1)
				{
					field_signature.add_candidate(constructed_signatures.front());
				}
			}

			// 5. CallbackDescriptor fields:
			if (is_callback_vft)
			{
				for (const auto& run : zero_store_runs)
				{
					if (run.qword_offsets.size() < 6)
						continue;
					field_callback_sig.add_candidate(run.qword_offsets[0]);
					if (run.trailing_scalar_size == 8)
						field_callback_async.add_candidate(run.trailing_scalar_offset);
				}
				std::size_t callback_vft_store_index = func_instructions.size();
				ZydisRegister callback_vft_register = ZYDIS_REGISTER_NONE;
				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					if (win_ops_has_strict_vft_lea(
							dinst.inst, dinst.operands.data(), code_address + dinst.offset,
							vft_sets.callback_vfts))
					{
						callback_vft_register = dinst.operands[0].reg.value;
						continue;
					}
					if (callback_vft_register != ZYDIS_REGISTER_NONE &&
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						dinst.operands[1].reg.value == callback_vft_register)
					{
						callback_vft_store_index = i;
						break;
					}
				}

				for (std::size_t i = callback_vft_store_index + 1;
					 i < func_instructions.size() && callback_vft_store_index < func_instructions.size();
					 ++i)
				{
					const auto& dinst = func_instructions[i];
					if (dinst.inst.mnemonic != ZYDIS_MNEMONIC_MOV ||
						dinst.operands[0].type != ZYDIS_OPERAND_TYPE_MEMORY ||
						dinst.operands[0].mem.base != this_reg ||
						!dinst.operands[0].mem.disp.has_displacement)
					{
						continue;
					}

					const auto disp = static_cast<std::ptrdiff_t>(dinst.operands[0].mem.disp.value);
					if (disp > 0 && dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						to_gpr32(dinst.operands[1].reg.value) == ZYDIS_REGISTER_R9)
					{
						field_callback_sig.add_candidate(disp);
					}
					if (disp > 0 && dinst.operands[0].size == 8 &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
						dinst.operands[1].imm.value.u <= 1)
					{
						field_callback_async.add_candidate(disp);
					}
				}
			}

			// 6. EventDescriptor fields:
			if (is_event_vft)
			{
				std::size_t event_vft_store_index = func_instructions.size();
				ZydisRegister event_vft_register = ZYDIS_REGISTER_NONE;
				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					if (win_ops_has_strict_vft_lea(
							dinst.inst, dinst.operands.data(), code_address + dinst.offset,
							vft_sets.event_vfts))
					{
						event_vft_register = dinst.operands[0].reg.value;
						continue;
					}
					if (event_vft_register != ZYDIS_REGISTER_NONE &&
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						dinst.operands[1].reg.value == event_vft_register)
					{
						event_vft_store_index = i;
						break;
					}
				}

				std::vector<std::ptrdiff_t> constructed_signals;
				for (std::size_t i = event_vft_store_index + 1;
					 i < func_instructions.size() && event_vft_store_index < func_instructions.size();
					 ++i)
				{
					const auto& dinst = func_instructions[i];
					if (dinst.inst.mnemonic != ZYDIS_MNEMONIC_LEA ||
						dinst.operands[0].type != ZYDIS_OPERAND_TYPE_REGISTER ||
						dinst.operands[1].type != ZYDIS_OPERAND_TYPE_MEMORY ||
						dinst.operands[1].mem.base != this_reg ||
						!dinst.operands[1].mem.disp.has_displacement)
					{
						continue;
					}

					const auto disp = static_cast<std::ptrdiff_t>(dinst.operands[1].mem.disp.value);
					if (disp > 0)
						constructed_signals.push_back(disp);
				}
				std::sort(constructed_signals.begin(), constructed_signals.end());
				constructed_signals.erase(
					std::unique(constructed_signals.begin(), constructed_signals.end()),
					constructed_signals.end());
				if (constructed_signals.size() == 1)
					field_event_signal.add_candidate(constructed_signals.front());
			}
			// 7. ClassDescriptor fields:
			if (is_class_vft)
			{
				std::vector<std::ptrdiff_t> raw_vector_offsets;

				std::vector<std::vector<ZydisRegister>> zero_state_at_inst(func_instructions.size());
				{
					std::vector<ZydisRegister> active_zero_regs;
					for (std::size_t k = 0; k < func_instructions.size(); ++k)
					{
						zero_state_at_inst[k] = active_zero_regs;
						const auto& dinst = func_instructions[k];
						bool is_zero_producer = false;
						ZydisRegister zero_producer_reg = ZYDIS_REGISTER_NONE;

						if ((dinst.inst.mnemonic == ZYDIS_MNEMONIC_XOR || dinst.inst.mnemonic == ZYDIS_MNEMONIC_SUB) &&
							dinst.inst.operand_count >= 2 &&
							dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							dinst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
						{
							const auto r0 = to_gpr32(dinst.operands[0].reg.value);
							const auto r1 = to_gpr32(dinst.operands[1].reg.value);
							if (r0 != ZYDIS_REGISTER_NONE && r0 == r1)
							{
								is_zero_producer = true;
								zero_producer_reg = r0;
							}
						}
						else if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
								 dinst.inst.operand_count >= 2 &&
								 dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								 dinst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
								 dinst.operands[1].imm.value.u == 0)
						{
							const auto r0 = to_gpr32(dinst.operands[0].reg.value);
							if (r0 != ZYDIS_REGISTER_NONE)
							{
								is_zero_producer = true;
								zero_producer_reg = r0;
							}
						}

						for (std::size_t op_idx = 0; op_idx < dinst.inst.operand_count; ++op_idx)
						{
							const auto& op = dinst.operands[op_idx];
							if (op.type == ZYDIS_OPERAND_TYPE_REGISTER &&
								(op.actions & ZYDIS_OPERAND_ACTION_MASK_WRITE) != 0)
							{
								const auto written_reg = to_gpr32(op.reg.value);
								if (written_reg != ZYDIS_REGISTER_NONE && written_reg != zero_producer_reg)
								{
									std::erase(active_zero_regs, written_reg);
								}
							}
						}

						if (is_zero_producer && zero_producer_reg != ZYDIS_REGISTER_NONE)
						{
							if (std::find(active_zero_regs.begin(), active_zero_regs.end(), zero_producer_reg) == active_zero_regs.end())
							{
								active_zero_regs.push_back(zero_producer_reg);
							}
						}
					}
				}

				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];
					std::ptrdiff_t cand_disp = -1;
					ZydisRegister temp_reg = ZYDIS_REGISTER_NONE;

					if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_LEA &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].mem.base == this_reg &&
						dinst.operands[1].mem.disp.has_displacement &&
						dinst.operands[1].mem.disp.value > 0 &&
						dinst.operands[1].mem.disp.value <= maximum_member_table_offset &&
						(dinst.operands[1].mem.disp.value % alignof(void*)) == 0)
					{
						temp_reg = dinst.operands[0].reg.value;
						cand_disp = static_cast<std::ptrdiff_t>(dinst.operands[1].mem.disp.value);
					}

					if (temp_reg != ZYDIS_REGISTER_NONE && cand_disp > 0)
					{
						bool store_0 = false, store_8 = false, store_10 = false;
						const std::size_t lookahead_limit = std::min(func_instructions.size(), i + 32);

						for (std::size_t j = i + 1; j < lookahead_limit; ++j)
						{
							const auto& sub_inst = func_instructions[j];
							if (sub_inst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								sub_inst.operands[0].reg.value == temp_reg &&
								sub_inst.inst.mnemonic != ZYDIS_MNEMONIC_CMP &&
								sub_inst.inst.mnemonic != ZYDIS_MNEMONIC_TEST)
							{
								break;
							}

							if (sub_inst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
								sub_inst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
								sub_inst.operands[0].mem.base == temp_reg)
							{
								const auto off = sub_inst.operands[0].mem.disp.has_displacement ? sub_inst.operands[0].mem.disp.value : 0;
								bool is_zero_store = false;

								if (sub_inst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE && sub_inst.operands[1].imm.value.u == 0)
								{
									is_zero_store = true;
								}
								else if (sub_inst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
								{
									const auto val_reg = to_gpr32(sub_inst.operands[1].reg.value);
									const auto& valid_zeros = zero_state_at_inst[j];
									if (std::find(valid_zeros.begin(), valid_zeros.end(), val_reg) != valid_zeros.end())
									{
										is_zero_store = true;
									}
								}

								if (is_zero_store || sub_inst.operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
								{
									if (off == 0) store_0 = true;
									else if (off == 8) store_8 = true;
									else if (off == 0x10) store_10 = true;
								}
							}
						}

						if (store_0 && store_8 && store_10)
						{
							raw_vector_offsets.push_back(cand_disp);
						}
					}
				}

				std::sort(raw_vector_offsets.begin(), raw_vector_offsets.end());
				raw_vector_offsets.erase(std::unique(raw_vector_offsets.begin(), raw_vector_offsets.end()), raw_vector_offsets.end());
				class_decoded_candidates += raw_vector_offsets.size();

				std::vector<std::array<std::ptrdiff_t, 5>> valid_runs;
				if (raw_vector_offsets.size() >= 5)
				{
					for (std::size_t i = 0; i < raw_vector_offsets.size(); ++i)
					{
						const auto o0 = raw_vector_offsets[i];
						if ((o0 % 8) != 0) continue;

						for (std::size_t j = i + 1; j < raw_vector_offsets.size(); ++j)
						{
							const auto o1 = raw_vector_offsets[j];
							const auto stride = o1 - o0;
							if (stride <= 0 || (stride % 8) != 0) continue;

							const auto o2 = o0 + 2 * stride;
							const auto o3 = o0 + 3 * stride;
							const auto o4 = o0 + 4 * stride;

							if (std::binary_search(raw_vector_offsets.begin(), raw_vector_offsets.end(), o2) &&
								std::binary_search(raw_vector_offsets.begin(), raw_vector_offsets.end(), o3) &&
								std::binary_search(raw_vector_offsets.begin(), raw_vector_offsets.end(), o4))
							{
								std::array<std::ptrdiff_t, 5> run = {o0, o1, o2, o3, o4};
								if (std::find(valid_runs.begin(), valid_runs.end(), run) == valid_runs.end())
								{
									valid_runs.push_back(run);
								}
							}
						}
					}
				}

				if (valid_runs.size() == 1)
				{
					if (class_containers && *class_containers != valid_runs[0])
					{
						return std::unexpected(CompatibilityError{
							.capability = capability_name,
							.failure = CompatibilityFailure::ambiguous_evidence,
							.matched_calls = matched_calls,
						});
					}
					class_containers = valid_runs[0];
				}
				else if (valid_runs.size() > 1)
				{
					return std::unexpected(CompatibilityError{
						.capability = capability_name,
						.failure = CompatibilityFailure::ambiguous_evidence,
						.matched_calls = matched_calls,
					});
				}

				const std::ptrdiff_t min_base_disp = class_containers ? (*class_containers)[4] + 0x18 : 0;
				for (std::size_t i = 0; i < func_instructions.size(); ++i)
				{
					const auto& dinst = func_instructions[i];

					if (dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
						dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						ZydisRegisterGetClass(dinst.operands[0].reg.value) == ZYDIS_REGCLASS_GPR64 &&
						dinst.operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						dinst.operands[1].mem.base != ZYDIS_REGISTER_NONE &&
						dinst.operands[1].mem.base != this_reg &&
						dinst.operands[1].mem.disp.has_displacement &&
						dinst.operands[1].mem.disp.value > min_base_disp &&
						dinst.operands[1].mem.disp.value <= maximum_member_table_offset &&
						(dinst.operands[1].mem.disp.value % alignof(void*)) == 0)
					{
						const auto load_reg = dinst.operands[0].reg.value;
						const auto disp = dinst.operands[1].mem.disp.value;

						const std::size_t lookahead = std::min(func_instructions.size(), i + 8);
						for (std::size_t j = i + 1; j < lookahead; ++j)
						{
							const auto& sub_inst = func_instructions[j];
							if (sub_inst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								sub_inst.operands[0].reg.value == load_reg &&
								sub_inst.inst.mnemonic != ZYDIS_MNEMONIC_TEST &&
								sub_inst.inst.mnemonic != ZYDIS_MNEMONIC_CMP)
							{
								break;
							}

							if (sub_inst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
								sub_inst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
								sub_inst.operands[0].mem.base == this_reg &&
								sub_inst.operands[0].mem.disp.has_displacement &&
								sub_inst.operands[0].mem.disp.value == disp &&
								sub_inst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								sub_inst.operands[1].reg.value == load_reg)
							{
								field_base_class.add_candidate(static_cast<std::ptrdiff_t>(disp));
								break;
							}
						}
					}

					const bool mask_operation =
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_OR ||
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_AND ||
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_TEST ||
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_CMP ||
						dinst.inst.mnemonic == ZYDIS_MNEMONIC_MOV;
					if (mask_operation)
					{
						if ((dinst.inst.mnemonic == ZYDIS_MNEMONIC_TEST ||
							 dinst.inst.mnemonic == ZYDIS_MNEMONIC_AND ||
							 dinst.inst.mnemonic == ZYDIS_MNEMONIC_OR) &&
							dinst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							dinst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
							(dinst.operands[1].imm.value.u & 0x8) != 0)
						{
							const auto masked_reg = to_gpr32(dinst.operands[0].reg.value);
							const std::size_t lookahead = std::min(func_instructions.size(), i + 8);
							for (std::size_t j = i + 1; j < lookahead; ++j)
							{
								const auto& sub_inst = func_instructions[j];
								if (sub_inst.inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
									sub_inst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
									sub_inst.operands[0].mem.base == this_reg &&
									sub_inst.operands[0].mem.disp.has_displacement &&
									sub_inst.operands[0].mem.disp.value > min_base_disp &&
									sub_inst.operands[0].mem.disp.value <= maximum_member_table_offset &&
									(sub_inst.operands[0].mem.disp.value % alignof(std::uint32_t)) == 0 &&
									sub_inst.operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
									to_gpr32(sub_inst.operands[1].reg.value) == masked_reg)
								{
									field_class_func.add_candidate(
										static_cast<std::ptrdiff_t>(sub_inst.operands[0].mem.disp.value));
									break;
								}
								if (sub_inst.operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
									to_gpr32(sub_inst.operands[0].reg.value) == masked_reg)
									break;
							}
						}
						if (dinst.operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
							dinst.operands[0].mem.base == this_reg &&
							dinst.operands[0].mem.disp.has_displacement &&
							dinst.operands[0].mem.disp.value > min_base_disp &&
							dinst.operands[0].mem.disp.value <= maximum_member_table_offset &&
							(dinst.operands[0].mem.disp.value % alignof(std::uint32_t)) == 0 &&
							dinst.operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
							(dinst.operands[1].imm.value.u & 0x8) != 0)
						{
							field_class_func.add_candidate(static_cast<std::ptrdiff_t>(dinst.operands[0].mem.disp.value));
						}
					}
				}
			}

			cursor += inst.length;
		}

		if (matched_calls == 0)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::missing_signature,
			}));
		}

		if (class_only)
		{
			if (field_base_class.has_conflict || field_class_func.has_conflict)
			{
				return std::unexpected(CompatibilityError{
					.capability = capability_name,
					.failure = CompatibilityFailure::ambiguous_evidence,
					.matched_calls = matched_calls,
				});
			}
			if (!class_containers)
			{
				return std::unexpected(CompatibilityError{
					.capability = "Reflection.MemberTable.Containers",
					.failure = CompatibilityFailure::insufficient_evidence,
					.matched_calls = matched_calls,
					.decoded_candidates = class_decoded_candidates,
				});
			}
			if (field_base_class.val == -1)
			{
				return std::unexpected(CompatibilityError{
					.capability = "Reflection.MemberTable.BaseClass",
					.failure = CompatibilityFailure::insufficient_evidence,
					.matched_calls = matched_calls,
				});
			}
			if (field_class_func.val == -1)
			{
				return std::unexpected(CompatibilityError{
					.capability = "Reflection.MemberTable.Functionality",
					.failure = CompatibilityFailure::insufficient_evidence,
					.matched_calls = matched_calls,
				});
			}
			return ReflectionLayoutEvidence{
				.descriptor_container_offsets = *class_containers,
				.base_class_offset = field_base_class.val,
				.functionality_offset = field_class_func.val,
				.supporting_calls = matched_calls,
				.matched_calls = matched_calls,
			};
		}

		std::vector<CompatibilityError> collected_errors;
		auto check_field = [&](bool has_conflict, bool is_missing, std::string_view cap_name) {
			if (has_conflict)
			{
				collected_errors.push_back(CompatibilityError{
					.capability = cap_name,
					.failure = CompatibilityFailure::ambiguous_evidence,
					.matched_calls = matched_calls,
				});
			}
			else if (is_missing)
			{
				collected_errors.push_back(CompatibilityError{
					.capability = cap_name,
					.failure = CompatibilityFailure::insufficient_evidence,
					.matched_calls = matched_calls,
				});
			}
		};

		check_field(field_name.has_conflict, field_name.val == -1, "Reflection.Descriptor.Name");
		check_field(field_owner.has_conflict, field_owner.val == -1, "Reflection.Member.Owner");
		check_field(field_security.has_conflict, field_security.val == -1, "Reflection.Member.Security");
		check_field(field_property_type.has_conflict, field_property_type.val == -1, "Reflection.Property.Type");
		check_field(field_property_func.has_conflict, field_property_func.val == -1, "Reflection.Property.Functionality");
		check_field(field_signature.has_conflict, field_signature.val == -1, "Reflection.Function.Signature");
		check_field(field_func_kind.has_conflict, field_func_kind.val == -1, "Reflection.Function.Kind");
		check_field(field_func_invoke.has_conflict, field_func_invoke.val == -1, "Reflection.Function.Invoke");
		check_field(field_func_this_delta.has_conflict, field_func_this_delta.val == -1, "Reflection.Function.ThisDelta");
		check_field(field_callback_sig.has_conflict, field_callback_sig.val == -1, "Reflection.Callback.Signature");
		check_field(field_callback_async.has_conflict, field_callback_async.val == -1, "Reflection.Callback.Async");
		check_field(field_event_signal.has_conflict, field_event_signal.val == -1, "Reflection.Event.Signal");
		check_field(field_base_class.has_conflict, field_base_class.val == -1, "Reflection.Class.Base");
		check_field(field_class_func.has_conflict, field_class_func.val == -1, "Reflection.Class.Functionality");
		if (!class_containers)
		{
			collected_errors.push_back(CompatibilityError{
				.capability = "Reflection.Class.Containers",
				.failure = CompatibilityFailure::insufficient_evidence,
				.matched_calls = matched_calls,
			});
		}

		if (!collected_errors.empty())
		{
			for (const auto& err : collected_errors)
			{
				emit(err);
			}
			return std::unexpected(collected_errors.front());
		}

		return ReflectionLayoutEvidence{
			.name_offset = field_name.val,
			.descriptor_container_offsets = *class_containers,
			.base_class_offset = field_base_class.val,
			.functionality_offset = field_class_func.val,
			.owner_offset = field_owner.val,
			.security_offset = field_security.val,
			.property_type_offset = field_property_type.val,
			.property_functionality_offset = field_property_func.val,
			.signature_offset = field_signature.val,
			.function_kind_offset = field_func_kind.val,
			.function_invoke_func_ptr_offset = field_func_invoke.val,
			.function_bound_this_delta_offset = field_func_this_delta.val,
			.callback_signature_offset = field_callback_sig.val,
			.callback_async_flag_offset = field_callback_async.val,
			.event_signal_offset = field_event_signal.val,
			.supporting_calls = matched_calls,
			.matched_calls = matched_calls,
		};
	}

	std::expected<InstanceLayoutEvidence, CompatibilityError> resolve_instance_layout(
		const std::span<const std::byte> executable_code,
		const std::uintptr_t code_address,
		const std::span<const std::byte> runtime_function_table,
		const std::uintptr_t module_address,
		const std::span<const std::uintptr_t> instance_vft_addresses,
		const std::span<const std::uintptr_t> instance_vft_entry_addresses,
		std::vector<CompatibilityError>* diagnostics) noexcept
	{
		auto emit = [diagnostics](CompatibilityError err) {
			if (diagnostics)
				diagnostics->push_back(err);
			return err;
		};

		if (executable_code.empty() || instance_vft_addresses.empty() || runtime_function_table.empty())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Instance.Layout",
				.failure = CompatibilityFailure::missing_signature,
			}));
		}
		ZydisDecoder decoder;
		if (!ZYAN_SUCCESS(ZydisDecoderInit(&decoder, ZYDIS_MACHINE_MODE_LONG_64, ZYDIS_STACK_WIDTH_64)))
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Instance.Layout",
				.failure = CompatibilityFailure::unsupported_instruction_form,
			}));
		}

		std::size_t matched_calls = 0;
		std::ptrdiff_t derived_parent = -1;
		std::ptrdiff_t derived_children = -1;
		std::ptrdiff_t derived_name = -1;

		std::vector<std::ptrdiff_t> trivial_getter_displacements;
		std::vector<std::ptrdiff_t> constructor_atom_store_displacements;
		for (const auto vft_fn_addr : instance_vft_entry_addresses)
		{
			if (vft_fn_addr < code_address || vft_fn_addr >= code_address + executable_code.size())
				continue;

			const auto fn_offset = static_cast<std::size_t>(vft_fn_addr - code_address);
			const auto bounds_res = find_function_bounds(
				executable_code, code_address, runtime_function_table, module_address, fn_offset);

			std::size_t fn_begin = fn_offset;
			std::size_t fn_end = std::min(executable_code.size(), fn_offset + 128);
			if (bounds_res)
			{
				fn_begin = bounds_res->begin;
				fn_end = bounds_res->end;
			}

			if (fn_end - fn_begin > 128)
				continue;

			bool has_call_or_branch = false;
			std::size_t ret_count = 0;
			std::ptrdiff_t single_load_disp = -1;
			std::size_t this_load_count = 0;

			RegTracker entry_tracker;
			std::array<std::ptrdiff_t, ZYDIS_REGISTER_MAX_VALUE + 1> reg_disp;
			reg_disp.fill(-1);
			reg_disp[ZYDIS_REGISTER_RCX] = -2;

			std::size_t scan_pos = fn_begin;
			while (scan_pos < fn_end && scan_pos + 1 <= executable_code.size())
			{
				const auto* bytes = reinterpret_cast<const std::uint8_t*>(executable_code.data() + scan_pos);
				ZydisDecodedInstruction inst;
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];

				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(&decoder, bytes, fn_end - scan_pos, &inst, operands)) || inst.length == 0)
				{
					scan_pos += 1;
					continue;
				}

				if (inst.mnemonic == ZYDIS_MNEMONIC_CALL ||
					inst.meta.category == ZYDIS_CATEGORY_COND_BR ||
					inst.meta.category == ZYDIS_CATEGORY_UNCOND_BR)
				{
					has_call_or_branch = true;
					break;
				}

				if (inst.mnemonic == ZYDIS_MNEMONIC_RET)
				{
					++ret_count;
					break;
				}

				if ((inst.mnemonic == ZYDIS_MNEMONIC_MOV || inst.mnemonic == ZYDIS_MNEMONIC_LEA) && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER && operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					operands[1].mem.disp.has_displacement)
				{
					const auto dst_reg = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, operands[0].reg.value);
					const auto base_reg = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, operands[1].mem.base);
					const auto base_role = entry_tracker.get_role(operands[1].mem.base);
					if (base_role == RegRole::This || reg_disp[base_reg] == -2)
					{
						const auto disp = static_cast<std::ptrdiff_t>(operands[1].mem.disp.value);
						if (disp >= 0x30 && disp <= 0x400 && disp % 4 == 0)
						{
							++this_load_count;
							single_load_disp = disp;
							reg_disp[dst_reg] = disp;
						}
					}
				}
				else if (inst.mnemonic == ZYDIS_MNEMONIC_MOV && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER && operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
				{
					const auto dst_reg = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, operands[0].reg.value);
					const auto src_reg = ZydisRegisterGetLargestEnclosing(ZYDIS_MACHINE_MODE_LONG_64, operands[1].reg.value);
					reg_disp[dst_reg] = reg_disp[src_reg];
				}

				entry_tracker.update(inst, operands, code_address + scan_pos);
				scan_pos += inst.length;
			}

			if (!has_call_or_branch && ret_count == 1 && (this_load_count == 1 || instance_vft_entry_addresses.size() == 1))
			{
				const auto rax_disp = reg_disp[ZYDIS_REGISTER_RAX];
				const auto candidate_disp = (rax_disp > 0) ? rax_disp : single_load_disp;
				if (candidate_disp > 0)
				{
					trivial_getter_displacements.push_back(candidate_disp);
				}
			}
		}

		const auto* code_bytes = reinterpret_cast<const std::uint8_t*>(executable_code.data());
		std::size_t cursor = 0;
		while (cursor + 7 <= executable_code.size())
		{
			const auto* opcode = static_cast<const std::uint8_t*>(
				std::memchr(code_bytes + cursor + 1, 0x8D, executable_code.size() - cursor - 1));
			if (opcode == nullptr)
				break;
			const auto opcode_offset = static_cast<std::size_t>(opcode - code_bytes);
			if (executable_code.size() - opcode_offset < 6)
				break;
			cursor = opcode_offset - 1;
			const auto* bytes = code_bytes + cursor;
			if ((bytes[0] & 0xF8) != 0x48 || (bytes[2] & 0xC7) != 0x05)
			{
				cursor += 2;
				continue;
			}
			std::int32_t displacement = 0;
			std::memcpy(&displacement, bytes + 3, sizeof(displacement));
			std::uintptr_t target = 0;
			if (!checked_add(code_address + cursor + 7, displacement, target) ||
				std::find(instance_vft_addresses.begin(), instance_vft_addresses.end(), target) ==
					instance_vft_addresses.end())
			{
				++cursor;
				continue;
			}
			ZydisDecodedInstruction inst;
			ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];

			if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(&decoder, bytes, executable_code.size() - cursor, &inst, operands)) || inst.length == 0)
			{
				cursor += 1;
				continue;
			}

			for (const auto vft_addr : instance_vft_addresses)
			{
				if (win_ops_has_vft_xref(inst, operands, code_address + cursor, vft_addr))
				{
					const auto bounds_res = find_function_bounds(
						executable_code, code_address, runtime_function_table, module_address, cursor);
					if (!bounds_res)
					{
						return std::unexpected(emit(CompatibilityError{
							.capability = "Instance.Layout",
							.failure = bounds_res.error(),
						}));
					}

					const auto bounds = *bounds_res;
					RegTracker tracker;

					struct ZeroFieldStore
					{
						std::size_t begin;
						std::size_t end;
						std::ptrdiff_t displacement;
						std::uint16_t size;
					};
					std::vector<ZeroFieldStore> zero_field_stores;
					std::ptrdiff_t fn_parent = -1;
					std::ptrdiff_t fn_children = -1;
					bool has_vft_store = false;
					std::size_t vft_store_position = 0;
					bool previous_was_call = false;
					std::ptrdiff_t pending_name_address_disp = -1;
					std::size_t pending_name_address_age = 0;

					std::size_t scan_pos = bounds.begin;
					while (scan_pos < bounds.end && scan_pos + 1 <= executable_code.size())
					{
						const auto* sbytes = reinterpret_cast<const std::uint8_t*>(executable_code.data() + scan_pos);
						ZydisDecodedInstruction sinst;
						ZydisDecodedOperand soperands[ZYDIS_MAX_OPERAND_COUNT];
						if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(&decoder, sbytes, bounds.end - scan_pos, &sinst, soperands)) || sinst.length == 0)
						{
							scan_pos += 1;
							continue;
						}

						if (win_ops_has_vft_xref(sinst, soperands, code_address + scan_pos, vft_addr))
						{
							tracker.set_role(soperands[0].reg.value, RegRole::VftAddr);
						}
						else if (sinst.mnemonic == ZYDIS_MNEMONIC_MOV && sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_MEMORY && soperands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							tracker.get_role(soperands[1].reg.value) == RegRole::VftAddr)
						{
							if (!has_vft_store)
								vft_store_position = scan_pos;
							has_vft_store = true;
							tracker.set_role(soperands[0].mem.base, RegRole::This);
						}
						else if (sinst.mnemonic == ZYDIS_MNEMONIC_MOV && sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
							tracker.get_role(soperands[0].mem.base) == RegRole::This &&
							soperands[0].mem.disp.has_displacement)
						{
							bool stores_zero = false;
							if (soperands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
								stores_zero = tracker.get_role(soperands[1].reg.value) == RegRole::Zero;
							else if (soperands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE)
								stores_zero = soperands[1].imm.value.u == 0;

							const auto disp = static_cast<std::ptrdiff_t>(soperands[0].mem.disp.value);
							if (stores_zero && disp > 0)
							{
								zero_field_stores.push_back(ZeroFieldStore{
									.begin = scan_pos,
									.end = scan_pos + sinst.length,
									.displacement = disp,
									.size = soperands[0].size,
								});
							}
						}
						if (sinst.mnemonic == ZYDIS_MNEMONIC_LEA && sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							to_gpr32(soperands[0].reg.value) == ZYDIS_REGISTER_RCX &&
							soperands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
							tracker.get_role(soperands[1].mem.base) == RegRole::This &&
							soperands[1].mem.disp.has_displacement)
						{
							pending_name_address_disp =
								static_cast<std::ptrdiff_t>(soperands[1].mem.disp.value);
							pending_name_address_age = 0;
						}
						else if (pending_name_address_disp > 0)
						{
							if (sinst.mnemonic == ZYDIS_MNEMONIC_CALL)
							{
								constructor_atom_store_displacements.push_back(pending_name_address_disp);
								pending_name_address_disp = -1;
							}
							else if (++pending_name_address_age > 2)
							{
								pending_name_address_disp = -1;
							}
						}
						if (previous_was_call && sinst.mnemonic == ZYDIS_MNEMONIC_MOV &&
							sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
							tracker.get_role(soperands[0].mem.base) == RegRole::This &&
							soperands[0].mem.disp.has_displacement &&
							soperands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							to_gpr32(soperands[1].reg.value) == ZYDIS_REGISTER_RAX)
						{
							constructor_atom_store_displacements.push_back(
								static_cast<std::ptrdiff_t>(soperands[0].mem.disp.value));
						}
						previous_was_call = sinst.mnemonic == ZYDIS_MNEMONIC_CALL;

						tracker.update(sinst, soperands, code_address + scan_pos);
						scan_pos += sinst.length;
					}

					if (has_vft_store)
					{
						// parent is a two-word ownership object immediately before the
						// Instance VFT store; children is the three-word ownership object
						// initialized immediately after it.
						for (std::size_t i = 0; i + 1 < zero_field_stores.size(); ++i)
						{
							const auto& first = zero_field_stores[i];
							const auto& second = zero_field_stores[i + 1];
							if (first.begin < vft_store_position && second.begin < vft_store_position &&
								first.size == 64 && second.size == 64 &&
								first.end == second.begin &&
								second.displacement == first.displacement +
									static_cast<std::ptrdiff_t>(sizeof(std::uintptr_t)))
							{
								fn_parent = first.displacement;
							}
						}
						for (std::size_t i = 0; i + 2 < zero_field_stores.size(); ++i)
						{
							const auto& first = zero_field_stores[i];
							const auto& second = zero_field_stores[i + 1];
							const auto& third = zero_field_stores[i + 2];
							if (first.begin > vft_store_position &&
								first.size == 64 && second.size == 64 && third.size == 64 &&
								first.end == second.begin && second.end == third.begin &&
								second.displacement == first.displacement +
									static_cast<std::ptrdiff_t>(sizeof(std::uintptr_t)) &&
								third.displacement == second.displacement +
									static_cast<std::ptrdiff_t>(sizeof(std::uintptr_t)))
							{
								fn_children = first.displacement;
								break;
							}
						}
					}

					if (has_vft_store)
					{
						++matched_calls;
						if (fn_parent > 0)
						{
							if (derived_parent != -1 && derived_parent != fn_parent)
								return std::unexpected(emit(CompatibilityError{
									.capability = "Instance.Layout",
									.failure = CompatibilityFailure::ambiguous_evidence,
								}));
							derived_parent = fn_parent;
						}
						if (fn_children > 0)
						{
							if (derived_children != -1 && derived_children != fn_children)
								return std::unexpected(emit(CompatibilityError{
									.capability = "Instance.Layout",
									.failure = CompatibilityFailure::ambiguous_evidence,
								}));
							derived_children = fn_children;
						}
					}
				}
			}

			cursor += inst.length;
		}

		std::sort(trivial_getter_displacements.begin(), trivial_getter_displacements.end());
		trivial_getter_displacements.erase(
			std::unique(trivial_getter_displacements.begin(), trivial_getter_displacements.end()),
			trivial_getter_displacements.end());
		std::sort(constructor_atom_store_displacements.begin(), constructor_atom_store_displacements.end());
		constructor_atom_store_displacements.erase(
			std::unique(constructor_atom_store_displacements.begin(), constructor_atom_store_displacements.end()),
			constructor_atom_store_displacements.end());
		std::size_t getter_index = 0;
		std::size_t store_index = 0;
		while (getter_index < trivial_getter_displacements.size() &&
			store_index < constructor_atom_store_displacements.size())
		{
			const auto getter_disp = trivial_getter_displacements[getter_index];
			const auto store_disp = constructor_atom_store_displacements[store_index];
			if (getter_disp < store_disp)
			{
				++getter_index;
			}
			else if (store_disp < getter_disp)
			{
				++store_index;
			}
			else
			{
				if (derived_name != -1)
				{
					return std::unexpected(emit(CompatibilityError{
						.capability = "Instance.Layout",
						.failure = CompatibilityFailure::ambiguous_evidence,
					}));
				}
				derived_name = getter_disp;
				++getter_index;
				++store_index;
			}
		}

		if (matched_calls == 0)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Instance.Layout",
				.failure = CompatibilityFailure::missing_signature,
			}));
		}

		if (derived_parent <= 0 || derived_children <= 0 || derived_name <= 0)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Instance.Layout",
				.failure = CompatibilityFailure::insufficient_evidence,
				.matched_calls = matched_calls,
				.decoded_candidates =
					(derived_parent > 0 ? 1u : 0u) |
					(derived_children > 0 ? 2u : 0u) |
					(derived_name > 0 ? 4u : 0u),
			}));
		}
		return InstanceLayoutEvidence{
			.parent_offset = derived_parent,
			.children_offset = derived_children,
			.name_offset = derived_name,
			.supporting_calls = matched_calls,
			.matched_calls = matched_calls,
		};
	}

	std::expected<SignalLayoutEvidence, CompatibilityError> resolve_signal_layout(
		const std::span<const std::byte> executable_code,
		const std::uintptr_t code_address,
		const std::span<const std::byte> runtime_function_table,
		const std::uintptr_t module_address,
		const std::uintptr_t signal_disconnect_address,
		const std::uintptr_t signal_slot_free_address,
		std::vector<CompatibilityError>* diagnostics) noexcept
	{
		auto emit = [diagnostics](CompatibilityError err) {
			if (diagnostics)
				diagnostics->push_back(err);
			return err;
		};
		auto fail = [&](const CompatibilityFailure failure, const std::size_t matched = 0,
						const std::size_t decoded = 0) {
			return std::unexpected(emit(CompatibilityError{
				.capability = "Signal.Layout",
				.failure = failure,
				.matched_calls = matched,
				.decoded_candidates = decoded,
			}));
		};

		if (executable_code.empty() || runtime_function_table.empty())
		{
			return fail(CompatibilityFailure::missing_signature);
		}

		ZydisDecoder decoder;
		if (!ZYAN_SUCCESS(ZydisDecoderInit(
				&decoder, ZYDIS_MACHINE_MODE_LONG_64, ZYDIS_STACK_WIDTH_64)))
		{
			return fail(CompatibilityFailure::unsupported_instruction_form);
		}

		std::vector<FunctionBounds> runtime_bounds;
		if (runtime_function_table.size() % sizeof(RuntimeFunctionEntry) != 0 ||
			code_address < module_address)
		{
			return fail(CompatibilityFailure::invalid_address_range);
		}
		for (std::size_t offset = 0; offset < runtime_function_table.size();
			 offset += sizeof(RuntimeFunctionEntry))
		{
			RuntimeFunctionEntry entry{};
			std::memcpy(&entry, runtime_function_table.data() + offset, sizeof(entry));
			if (entry.begin_address == 0 && entry.end_address == 0 && entry.unwind_info_address == 0)
				continue;
			if (entry.begin_address >= entry.end_address)
				return fail(CompatibilityFailure::invalid_address_range);
			std::uintptr_t begin_address = 0;
			std::uintptr_t end_address = 0;
			if (!checked_add(module_address, entry.begin_address, begin_address) ||
				!checked_add(module_address, entry.end_address, end_address))
			{
				return fail(CompatibilityFailure::invalid_address_range);
			}
			if (begin_address < code_address || end_address > code_address + executable_code.size())
				continue;
			runtime_bounds.push_back(FunctionBounds{
				.begin = static_cast<std::size_t>(begin_address - code_address),
				.end = static_cast<std::size_t>(end_address - code_address),
			});
		}
		std::sort(runtime_bounds.begin(), runtime_bounds.end());
		runtime_bounds.erase(std::unique(runtime_bounds.begin(), runtime_bounds.end()), runtime_bounds.end());

		auto bounds_for = [&](const std::uintptr_t address) -> std::optional<FunctionBounds> {
			if (address < code_address || address >= code_address + executable_code.size())
				return std::nullopt;
			const auto offset = static_cast<std::size_t>(address - code_address);
			auto it = std::upper_bound(
				runtime_bounds.begin(), runtime_bounds.end(), FunctionBounds{offset, std::numeric_limits<std::size_t>::max()});
			if (it == runtime_bounds.begin())
				return std::nullopt;
			--it;
			return offset >= it->begin && offset < it->end ? std::optional{*it} : std::nullopt;
		};
		auto direct_call_target = [&](const ZydisDecodedInstruction& inst,
									  const ZydisDecodedOperand* operands,
									  const std::size_t offset) -> std::optional<std::uintptr_t> {
			if (inst.mnemonic != ZYDIS_MNEMONIC_CALL || inst.operand_count < 1 ||
				operands[0].type != ZYDIS_OPERAND_TYPE_IMMEDIATE ||
				!operands[0].imm.is_relative)
			{
				return std::nullopt;
			}
			std::uintptr_t target = 0;
			if (!checked_add(code_address + offset + inst.length, operands[0].imm.value.s, target) ||
				target < code_address || target >= code_address + executable_code.size())
			{
				return std::nullopt;
			}
			return target;
		};
		auto append_unique = [](std::vector<std::ptrdiff_t>& values, const std::ptrdiff_t value) {
			if (value >= 0 && std::find(values.begin(), values.end(), value) == values.end())
				values.push_back(value);
		};

		std::size_t disconnect_matches = 0;
		std::uintptr_t disconnect_address = signal_disconnect_address;
		if (disconnect_address == 0)
			disconnect_address = pattern_scan_signal_disconnect(
				executable_code, code_address, &disconnect_matches);
		if (signal_disconnect_address == 0 && disconnect_matches > 1)
			return fail(CompatibilityFailure::ambiguous_evidence, disconnect_matches);

		std::size_t free_matches = 0;
		std::uintptr_t free_address = signal_slot_free_address;
		if (free_address == 0)
			free_address = pattern_scan_signal_slot_free(
				executable_code, code_address, &free_matches);
		if (signal_slot_free_address == 0 && free_matches > 1)
			return fail(CompatibilityFailure::ambiguous_evidence, free_matches);

		std::size_t insert_matches = 0;
		const auto insert_address = pattern_scan_signal_slot_insert(
			executable_code, code_address, &insert_matches);
		if (insert_matches > 1)
			return fail(CompatibilityFailure::ambiguous_evidence, insert_matches);

		const auto disconnect_bounds = bounds_for(disconnect_address);
		const auto free_bounds = bounds_for(free_address);
		const auto insert_bounds = bounds_for(insert_address);
		if (!disconnect_bounds || !free_bounds || !insert_bounds)
			return fail(CompatibilityFailure::missing_signature);

		struct UnlinkSeed
		{
			std::ptrdiff_t source{-1};
			std::uintptr_t helper{};

			constexpr auto operator<=>(const UnlinkSeed&) const noexcept = default;
		};
		std::vector<UnlinkSeed> unlink_seeds;
		{
			RegTracker tracker;
			tracker.initial_rcx = RegRole::SlotPtr;
			tracker.set_role(ZYDIS_REGISTER_RCX, RegRole::SlotPtr);
			std::size_t position = disconnect_bounds->begin;
			while (position < disconnect_bounds->end)
			{
				const auto* bytes = reinterpret_cast<const std::uint8_t*>(
					executable_code.data() + position);
				ZydisDecodedInstruction inst;
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];
				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
						&decoder, bytes, disconnect_bounds->end - position, &inst, operands)) ||
					inst.length == 0)
				{
					return fail(CompatibilityFailure::unsupported_instruction_form);
				}
				if (const auto target = direct_call_target(inst, operands, position);
					target && tracker.get_role(ZYDIS_REGISTER_RCX) == RegRole::SlotField &&
					tracker.get_role(ZYDIS_REGISTER_RDX) == RegRole::SlotPtr)
				{
					const auto source = static_cast<std::ptrdiff_t>(
						tracker.get_imm(ZYDIS_REGISTER_RCX));
					if (source > 0 && bounds_for(*target))
						unlink_seeds.push_back(UnlinkSeed{source, *target});
				}
				tracker.update(inst, operands, code_address + position);
				position += inst.length;
			}
		}
		std::sort(unlink_seeds.begin(), unlink_seeds.end());
		unlink_seeds.erase(std::unique(unlink_seeds.begin(), unlink_seeds.end()), unlink_seeds.end());
		if (unlink_seeds.size() != 1)
		{
			return fail(
				unlink_seeds.empty() ? CompatibilityFailure::insufficient_evidence
									 : CompatibilityFailure::ambiguous_evidence,
				1, unlink_seeds.size());
		}

		struct Topology
		{
			std::ptrdiff_t source{-1};
			std::ptrdiff_t head{-1};
			std::ptrdiff_t next{-1};
			std::ptrdiff_t destroy{-1};

			constexpr auto operator<=>(const Topology&) const noexcept = default;
		};
		Topology unlink_topology{.source = unlink_seeds.front().source};
		{
			const auto helper_bounds = bounds_for(unlink_seeds.front().helper);
			if (!helper_bounds)
				return fail(CompatibilityFailure::invalid_address_range, 1);
			RegTracker tracker;
			tracker.initial_rcx = RegRole::SignalPtr;
			tracker.set_role(ZYDIS_REGISTER_RCX, RegRole::SignalPtr);
			tracker.set_role(ZYDIS_REGISTER_RDX, RegRole::SlotPtr);
			std::vector<std::ptrdiff_t> signal_reads;
			std::vector<std::ptrdiff_t> signal_writes;
			std::vector<std::ptrdiff_t> slot_zero_writes;
			std::vector<std::ptrdiff_t> destroy_calls;
			std::size_t position = helper_bounds->begin;
			while (position < helper_bounds->end)
			{
				const auto* bytes = reinterpret_cast<const std::uint8_t*>(
					executable_code.data() + position);
				ZydisDecodedInstruction inst;
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];
				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
						&decoder, bytes, helper_bounds->end - position, &inst, operands)) ||
					inst.length == 0)
				{
					return fail(CompatibilityFailure::unsupported_instruction_form, 1);
				}
				if (inst.operand_count >= 2 &&
					(inst.mnemonic == ZYDIS_MNEMONIC_MOV || inst.mnemonic == ZYDIS_MNEMONIC_LEA))
				{
					if (operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						operands[1].size == 64 && operands[1].mem.disp.has_displacement &&
						tracker.get_role(operands[1].mem.base) == RegRole::SignalPtr)
					{
						append_unique(signal_reads, static_cast<std::ptrdiff_t>(
							operands[1].mem.disp.value));
					}
					if (operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						operands[0].size == 64 && operands[0].mem.disp.has_displacement)
					{
						const auto base_role = tracker.get_role(operands[0].mem.base);
						const auto disp = static_cast<std::ptrdiff_t>(operands[0].mem.disp.value);
						if (base_role == RegRole::SignalPtr)
							append_unique(signal_writes, disp);
						if (base_role == RegRole::SlotPtr)
						{
							const bool zero =
								(operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
								 operands[1].imm.value.u == 0) ||
								(operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								 tracker.get_role(operands[1].reg.value) == RegRole::Zero);
							if (zero)
								append_unique(slot_zero_writes, disp);
						}
					}
				}
				if (inst.mnemonic == ZYDIS_MNEMONIC_MOV && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					operands[1].mem.disp.has_displacement &&
					tracker.get_role(operands[1].mem.base) == RegRole::SignalField)
				{
					tracker.set_role(
						operands[0].reg.value,
						RegRole::SlotField,
						static_cast<std::uint64_t>(operands[1].mem.disp.value));
					position += inst.length;
					continue;
				}
				if (inst.mnemonic == ZYDIS_MNEMONIC_CALL && inst.operand_count >= 1 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					tracker.get_role(operands[0].reg.value) == RegRole::SlotField)
				{
					append_unique(destroy_calls, static_cast<std::ptrdiff_t>(
						tracker.get_imm(operands[0].reg.value)));
				}
				tracker.update(inst, operands, code_address + position);
				position += inst.length;
			}
			std::vector<std::ptrdiff_t> head_candidates;
			for (const auto disp : signal_reads)
				if (std::find(signal_writes.begin(), signal_writes.end(), disp) != signal_writes.end())
					head_candidates.push_back(disp);
			std::vector<std::ptrdiff_t> next_candidates;
			for (const auto disp : slot_zero_writes)
				if (disp != unlink_topology.source)
					next_candidates.push_back(disp);
			if (head_candidates.size() != 1 || next_candidates.size() != 1 ||
				destroy_calls.size() != 1 || head_candidates.front() <= 0 ||
				next_candidates.front() <= 0 || destroy_calls.front() <= 0)
			{
				return fail(CompatibilityFailure::insufficient_evidence, 2,
					head_candidates.size() + next_candidates.size() + destroy_calls.size());
			}
			unlink_topology.head = head_candidates.front();
			unlink_topology.next = next_candidates.front();
			unlink_topology.destroy = destroy_calls.front();
		}

		std::vector<std::uintptr_t> insert_callers;
		const auto* raw_code = reinterpret_cast<const std::uint8_t*>(executable_code.data());
		for (std::size_t position = 0; position + 5 <= executable_code.size(); ++position)
		{
			if (raw_code[position] != 0xE8)
				continue;

			std::int32_t displacement = 0;
			std::memcpy(&displacement, raw_code + position + 1, sizeof(displacement));
			std::uintptr_t target = 0;
			if (!checked_add(
					code_address + position + 5,
					static_cast<std::int64_t>(displacement),
					target) ||
				target != insert_address)
			{
				continue;
			}

			if (const auto canonical = bounds_for(code_address + position);
				canonical && canonical->end - canonical->begin <= 0x4000)
			{
				insert_callers.push_back(code_address + canonical->begin);
			}
		}
		std::sort(insert_callers.begin(), insert_callers.end());
		insert_callers.erase(std::unique(insert_callers.begin(), insert_callers.end()),
			insert_callers.end());
		std::erase_if(insert_callers, [&](const std::uintptr_t function_address) {
			const auto function_bounds = bounds_for(function_address);
			if (!function_bounds)
				return true;
			std::size_t position = function_bounds->begin;
			while (position < function_bounds->end)
			{
				const auto* bytes = reinterpret_cast<const std::uint8_t*>(
					executable_code.data() + position);
				ZydisDecodedInstruction inst;
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];
				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
						&decoder, bytes, function_bounds->end - position, &inst, operands)) ||
					inst.length == 0)
				{
					return true;
				}
				if (direct_call_target(inst, operands, position) == insert_address)
					return false;
				position += inst.length;
			}
			return true;
		});
		if (insert_callers.empty())
			return fail(CompatibilityFailure::insufficient_evidence, 2);

		struct ConnectCandidate
		{
			std::ptrdiff_t event_signal{-1};
			std::ptrdiff_t source{-1};
			std::ptrdiff_t wrapper{-1};
			std::ptrdiff_t wrapper_rep{-1};
			std::ptrdiff_t weak{-1};
			std::size_t allocation_size{};
			std::uintptr_t insert_helper{};
			std::vector<std::ptrdiff_t> zero_dwords;
			std::vector<std::ptrdiff_t> zero_qwords;
		};
		std::vector<ConnectCandidate> connect_candidates;
		std::vector<std::uintptr_t> connect_functions = insert_callers;
		for (const auto function_address : connect_functions)
		{
			const auto function_bounds = bounds_for(function_address);
			if (!function_bounds || function_bounds->end - function_bounds->begin > 0x4000)
				continue;
			RegTracker tracker;
			ConnectCandidate candidate;
			std::size_t pending_allocation_size = 0;
			bool has_allocation = false;
			bool insertion_seen = false;
			std::vector<std::ptrdiff_t> one_dwords;
			std::vector<std::ptrdiff_t> weak_increments;
			auto finish_candidate = [&]() {
				if (!has_allocation)
					return;
				if (candidate.insert_helper == 0 || candidate.source <= 0 ||
					candidate.wrapper < 0 ||
					candidate.wrapper_rep != candidate.wrapper +
						static_cast<std::ptrdiff_t>(sizeof(void*)) ||
					static_cast<std::size_t>(candidate.wrapper_rep + sizeof(void*)) !=
						candidate.allocation_size)
				{
					return;
				}
				std::vector<std::ptrdiff_t> weak_candidates;
				for (const auto disp : one_dwords)
					if (std::find(weak_increments.begin(), weak_increments.end(), disp) !=
						weak_increments.end())
						weak_candidates.push_back(disp);
				if (weak_candidates.size() != 1)
					return;
				candidate.weak = weak_candidates.front();
				connect_candidates.push_back(candidate);
			};
			ZydisRegister pending_wrapper_base = ZYDIS_REGISTER_NONE;
			ZydisRegister pending_wrapper_dest = ZYDIS_REGISTER_NONE;
			std::size_t position = function_bounds->begin;
			bool decode_failed = false;
			while (position < function_bounds->end)
			{
				const auto* bytes = reinterpret_cast<const std::uint8_t*>(
					executable_code.data() + position);
				ZydisDecodedInstruction inst;
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];
				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
						&decoder, bytes, function_bounds->end - position, &inst, operands)) ||
					inst.length == 0)
				{
					decode_failed = true;
					break;
				}

				if ((inst.mnemonic == ZYDIS_MNEMONIC_MOV ||
					 inst.mnemonic == ZYDIS_MNEMONIC_MOVSX) &&
					inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					operands[1].mem.disp.has_displacement &&
					operands[1].size == 32 &&
					tracker.get_role(operands[1].mem.base) == RegRole::This)
				{
					tracker.set_role(
						operands[0].reg.value,
						RegRole::EventSignalField,
						static_cast<std::uint64_t>(operands[1].mem.disp.value));
					position += inst.length;
					continue;
				}
				if (inst.mnemonic == ZYDIS_MNEMONIC_ADD && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					tracker.get_role(operands[0].reg.value) == RegRole::EventSignalField &&
					tracker.get_role(operands[1].reg.value) == RegRole::Arg1)
				{
					const auto disp = tracker.get_imm(operands[0].reg.value);
					tracker.set_role(operands[0].reg.value, RegRole::SignalAddress, disp);
					position += inst.length;
					continue;
				}
				if ((inst.mnemonic == ZYDIS_MNEMONIC_MOV ||
					 inst.mnemonic == ZYDIS_MNEMONIC_MOVSX) &&
					inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					tracker.get_role(operands[1].mem.base) == RegRole::SignalAddress &&
					(!operands[1].mem.disp.has_displacement || operands[1].mem.disp.value == 0))
				{
					tracker.set_role(
						operands[0].reg.value,
						RegRole::SignalObject,
						tracker.get_imm(operands[1].mem.base));
					position += inst.length;
					continue;
				}

				if (inst.mnemonic == ZYDIS_MNEMONIC_MOV && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
				{
					const auto base = to_gpr32(operands[1].mem.base);
					const auto disp = operands[1].mem.disp.has_displacement
						? static_cast<std::ptrdiff_t>(operands[1].mem.disp.value) : 0;
					if (disp == static_cast<std::ptrdiff_t>(sizeof(void*)) &&
						base == pending_wrapper_base &&
						pending_wrapper_dest != ZYDIS_REGISTER_NONE)
					{
						tracker.set_role(pending_wrapper_dest, RegRole::WrapperPtr);
						tracker.set_role(operands[0].reg.value, RegRole::WrapperRep);
						pending_wrapper_base = ZYDIS_REGISTER_NONE;
						pending_wrapper_dest = ZYDIS_REGISTER_NONE;
						position += inst.length;
						continue;
					}
					if (disp == 0 && base != ZYDIS_REGISTER_RSP && base != ZYDIS_REGISTER_RBP)
					{
						pending_wrapper_base = base;
						pending_wrapper_dest = operands[0].reg.value;
					}
					else
					{
						pending_wrapper_base = ZYDIS_REGISTER_NONE;
						pending_wrapper_dest = ZYDIS_REGISTER_NONE;
					}
				}

				if (inst.mnemonic == ZYDIS_MNEMONIC_MOV && inst.operand_count >= 2 &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
					to_gpr32(operands[0].reg.value) == ZYDIS_REGISTER_RCX &&
					operands[1].imm.value.u >= 32 && operands[1].imm.value.u <= 256)
				{
					pending_allocation_size = static_cast<std::size_t>(operands[1].imm.value.u);
				}

				if (inst.mnemonic == ZYDIS_MNEMONIC_CALL)
				{
					if (pending_allocation_size != 0)
					{
						finish_candidate();
						tracker.handle_call();
						tracker.set_role(ZYDIS_REGISTER_RAX, RegRole::AllocatedSlot);
						candidate = ConnectCandidate{.allocation_size = pending_allocation_size};
						has_allocation = true;
						insertion_seen = false;
						one_dwords.clear();
						weak_increments.clear();
						pending_allocation_size = 0;
						position += inst.length;
						continue;
					}
					if (has_allocation &&
						(tracker.get_role(ZYDIS_REGISTER_RDX) == RegRole::AllocatedSlot ||
						 tracker.get_role(ZYDIS_REGISTER_RDX) == RegRole::SlotPtr))
					{
						if (const auto target = direct_call_target(inst, operands, position);
							target && *target == insert_address)
						{
							candidate.insert_helper = *target;
							insertion_seen = true;
						}
					}
				}

				if (has_allocation && inst.operand_count >= 2 &&
					inst.mnemonic == ZYDIS_MNEMONIC_MOV &&
					operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY)
				{
					const auto base_role = tracker.get_role(operands[0].mem.base);
					if (base_role == RegRole::AllocatedSlot || base_role == RegRole::SlotPtr)
					{
						const auto disp = operands[0].mem.disp.has_displacement
							? static_cast<std::ptrdiff_t>(operands[0].mem.disp.value) : 0;
						if (disp >= 0 && static_cast<std::size_t>(disp) < candidate.allocation_size)
						{
							const auto source_role = operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER
								? tracker.get_role(operands[1].reg.value) : RegRole::Unknown;
							const bool zero =
								(operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
								 operands[1].imm.value.u == 0) ||
								(operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								 source_role == RegRole::Zero);
							const bool one =
								(operands[1].type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
								 operands[1].imm.value.u == 1) ||
								(operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
								 source_role == RegRole::ImmVal &&
								 tracker.get_imm(operands[1].reg.value) == 1);
							if (operands[0].size == 32 && zero)
								append_unique(candidate.zero_dwords, disp);
							if (operands[0].size == 32 && one)
								append_unique(one_dwords, disp);
							if (operands[0].size == 64 && zero)
								append_unique(candidate.zero_qwords, disp);
							if (operands[0].size == 64 && !zero &&
								disp == unlink_topology.source)
							{
								candidate.source = disp;
							}
							if (operands[0].size == 64 && source_role == RegRole::WrapperPtr)
							{
								if (candidate.wrapper != -1 && candidate.wrapper != disp)
									decode_failed = true;
								candidate.wrapper = disp;
							}
							if (operands[0].size == 64 && source_role == RegRole::WrapperRep)
							{
								if (candidate.wrapper_rep != -1 && candidate.wrapper_rep != disp)
									decode_failed = true;
								candidate.wrapper_rep = disp;
							}
						}
					}
				}
				if (has_allocation && insertion_seen &&
					(inst.mnemonic == ZYDIS_MNEMONIC_INC ||
					 inst.mnemonic == ZYDIS_MNEMONIC_XADD) &&
					inst.operand_count >= 1 && operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					operands[0].size == 32)
				{
					const auto base_role = tracker.get_role(operands[0].mem.base);
					if (base_role == RegRole::AllocatedSlot || base_role == RegRole::SlotPtr)
					{
						append_unique(weak_increments,
							operands[0].mem.disp.has_displacement
								? static_cast<std::ptrdiff_t>(operands[0].mem.disp.value) : 0);
					}
				}

				if (decode_failed)
					break;
				tracker.update(inst, operands, code_address + position);
				position += inst.length;
			}
			if (!decode_failed)
				finish_candidate();
		}

		struct InsertCandidate
		{
			std::ptrdiff_t head{-1};
			std::ptrdiff_t strong{-1};
			std::ptrdiff_t next{-1};
		};
		auto analyze_insert = [&](const std::uintptr_t helper) -> std::optional<InsertCandidate> {
			const auto helper_bounds = bounds_for(helper);
			if (!helper_bounds)
				return std::nullopt;
			RegTracker tracker;
			tracker.initial_rcx = RegRole::SignalPtr;
			tracker.set_role(ZYDIS_REGISTER_RCX, RegRole::SignalPtr);
			tracker.set_role(ZYDIS_REGISTER_RDX, RegRole::SlotPtr);
			std::vector<std::ptrdiff_t> signal_reads;
			std::vector<std::ptrdiff_t> signal_writes;
			std::vector<std::ptrdiff_t> strong_candidates;
			std::vector<std::ptrdiff_t> next_candidates;
			std::size_t position = helper_bounds->begin;
			while (position < helper_bounds->end)
			{
				const auto* bytes = reinterpret_cast<const std::uint8_t*>(
					executable_code.data() + position);
				ZydisDecodedInstruction inst;
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];
				if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
						&decoder, bytes, helper_bounds->end - position, &inst, operands)) ||
					inst.length == 0)
				{
					return std::nullopt;
				}
				if (inst.operand_count >= 2 &&
					(inst.mnemonic == ZYDIS_MNEMONIC_MOV || inst.mnemonic == ZYDIS_MNEMONIC_LEA))
				{
					if (operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						operands[1].size == 64 && operands[1].mem.disp.has_displacement &&
						tracker.get_role(operands[1].mem.base) == RegRole::SignalPtr)
					{
						append_unique(signal_reads, static_cast<std::ptrdiff_t>(
							operands[1].mem.disp.value));
					}
					if (operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						operands[0].size == 64 && operands[0].mem.disp.has_displacement &&
						tracker.get_role(operands[0].mem.base) == RegRole::SignalPtr)
					{
						append_unique(signal_writes, static_cast<std::ptrdiff_t>(
							operands[0].mem.disp.value));
					}
					if (inst.mnemonic == ZYDIS_MNEMONIC_LEA &&
						operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
						operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
						operands[1].mem.disp.has_displacement &&
						tracker.get_role(operands[1].mem.base) == RegRole::SlotPtr)
					{
						tracker.set_role(
							operands[0].reg.value,
							RegRole::SlotField,
							static_cast<std::uint64_t>(operands[1].mem.disp.value));
						position += inst.length;
						continue;
					}
				}
				if ((inst.mnemonic == ZYDIS_MNEMONIC_INC ||
					 inst.mnemonic == ZYDIS_MNEMONIC_XADD) &&
					inst.operand_count >= 1 && operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					operands[0].size == 32 &&
					tracker.get_role(operands[0].mem.base) == RegRole::SlotPtr)
				{
					append_unique(strong_candidates,
						operands[0].mem.disp.has_displacement
							? static_cast<std::ptrdiff_t>(operands[0].mem.disp.value) : 0);
				}
				if (inst.mnemonic == ZYDIS_MNEMONIC_CALL &&
					tracker.get_role(ZYDIS_REGISTER_RCX) == RegRole::SlotField &&
					direct_call_target(inst, operands, position))
				{
					append_unique(next_candidates, static_cast<std::ptrdiff_t>(
						tracker.get_imm(ZYDIS_REGISTER_RCX)));
				}
				tracker.update(inst, operands, code_address + position);
				position += inst.length;
			}
			std::vector<std::ptrdiff_t> head_candidates;
			for (const auto disp : signal_reads)
				if (std::find(signal_writes.begin(), signal_writes.end(), disp) != signal_writes.end())
					head_candidates.push_back(disp);
			if (head_candidates.size() != 1 || strong_candidates.size() != 1 ||
				next_candidates.size() != 1)
			{
				return std::nullopt;
			}
			return InsertCandidate{
				.head = head_candidates.front(),
				.strong = strong_candidates.front(),
				.next = next_candidates.front(),
			};
		};

		struct CompleteCandidate
		{
			std::ptrdiff_t head{-1};
			std::ptrdiff_t strong{-1};
			std::ptrdiff_t weak{-1};
			std::ptrdiff_t next{-1};
			std::ptrdiff_t source{-1};
			std::ptrdiff_t wrapper{-1};

			constexpr auto operator<=>(const CompleteCandidate&) const noexcept = default;
		};
		std::vector<CompleteCandidate> complete_candidates;
		for (const auto& connect : connect_candidates)
		{
			const auto insert = analyze_insert(connect.insert_helper);
			if (!insert || connect.source != unlink_topology.source ||
				connect.wrapper != unlink_topology.destroy +
					static_cast<std::ptrdiff_t>(sizeof(void*)) ||
				insert->head != unlink_topology.head || insert->next != unlink_topology.next ||
				std::find(connect.zero_dwords.begin(), connect.zero_dwords.end(), insert->strong) ==
					connect.zero_dwords.end() ||
				std::find(connect.zero_qwords.begin(), connect.zero_qwords.end(), insert->next) ==
					connect.zero_qwords.end() ||
				insert->strong == connect.weak)
			{
				continue;
			}
			complete_candidates.push_back(CompleteCandidate{
				.head = insert->head,
				.strong = insert->strong,
				.weak = connect.weak,
				.next = insert->next,
				.source = connect.source,
				.wrapper = connect.wrapper,
			});
		}
		if (complete_candidates.empty())
			return fail(CompatibilityFailure::insufficient_evidence, 3, connect_candidates.size());
		const auto resolved = complete_candidates.front();
		for (const auto& candidate : complete_candidates)
		{
			if (candidate != resolved)
				return fail(CompatibilityFailure::ambiguous_evidence,
					3 + complete_candidates.size(), complete_candidates.size());
		}

		return SignalLayoutEvidence{
			.signal_head_offset = resolved.head,
			.slot_strong_offset = resolved.strong,
			.slot_weak_offset = resolved.weak,
			.slot_next_offset = resolved.next,
			.slot_source_offset = resolved.source,
			.slot_wrapper_ptr_offset = resolved.wrapper,
			.supporting_calls = 3 + complete_candidates.size(),
			.matched_calls = 3 + connect_candidates.size(),
		};
	}

	std::expected<JobLayoutEvidence, CompatibilityError> resolve_job_layout(
		const std::span<const std::byte> executable_code,
		const std::uintptr_t code_address,
		const std::span<const std::byte> runtime_function_table,
		const std::uintptr_t module_address,
		const std::span<const std::uintptr_t> waiting_scripts_job_vft_addresses,
		std::vector<CompatibilityError>* diagnostics) noexcept
	{
		auto emit = [diagnostics](CompatibilityError err) {
			if (diagnostics)
				diagnostics->push_back(err);
			return err;
		};

		if (executable_code.empty() || waiting_scripts_job_vft_addresses.empty() || runtime_function_table.empty())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Job.Layout",
				.failure = CompatibilityFailure::missing_signature,
			}));
		}
		ZydisDecoder decoder;
		if (!ZYAN_SUCCESS(ZydisDecoderInit(&decoder, ZYDIS_MACHINE_MODE_LONG_64, ZYDIS_STACK_WIDTH_64)))
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Job.Layout",
				.failure = CompatibilityFailure::unsupported_instruction_form,
			}));
		}
		std::ptrdiff_t derived_script_ctx = -1;
		std::size_t matched_calls = 0;
		std::vector<std::uintptr_t> waiting_vfts(
			waiting_scripts_job_vft_addresses.begin(), waiting_scripts_job_vft_addresses.end());
		std::ranges::sort(waiting_vfts);
		waiting_vfts.erase(std::ranges::unique(waiting_vfts).begin(), waiting_vfts.end());

		const auto* code_bytes = reinterpret_cast<const std::uint8_t*>(executable_code.data());
		for (std::size_t cursor = 0; cursor + 7 <= executable_code.size(); ++cursor)
		{
			// MSVC materializes vft addresses as a canonical seven-byte
			// REX.W LEA/MOV from RIP. Resolve that cheap fixed-width form
			// before invoking Zydis; decoding every instruction in Studio's
			// full .text section made bootstrap exceed the bridge deadline.
			if ((code_bytes[cursor] & 0xF8) != 0x48 ||
				(code_bytes[cursor + 1] != 0x8D && code_bytes[cursor + 1] != 0x8B) ||
				(code_bytes[cursor + 2] & 0xC7) != 0x05)
			{
				continue;
			}

			std::int32_t displacement = 0;
			std::memcpy(&displacement, code_bytes + cursor + 3, sizeof(displacement));
			std::uintptr_t instruction_address = 0;
			std::uintptr_t instruction_end = 0;
			std::uintptr_t waiting_vft_addr = 0;
			if (cursor > static_cast<std::size_t>((std::numeric_limits<std::ptrdiff_t>::max)()) ||
				!checked_add(code_address, static_cast<std::ptrdiff_t>(cursor), instruction_address) ||
				!checked_add(instruction_address, 7, instruction_end) ||
				!checked_add(instruction_end, displacement, waiting_vft_addr) ||
				!std::ranges::binary_search(waiting_vfts, waiting_vft_addr))
			{
				continue;
			}

			ZydisDecodedInstruction inst;
			ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT];
			if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(
					&decoder, code_bytes + cursor, executable_code.size() - cursor, &inst, operands)) ||
				inst.length != 7 ||
				!win_ops_has_vft_xref(inst, operands, instruction_address, waiting_vft_addr))
			{
				continue;
			}

			const auto bounds_res = find_function_bounds(
						executable_code, code_address, runtime_function_table, module_address, cursor);
					if (!bounds_res)
					{
						return std::unexpected(emit(CompatibilityError{
							.capability = "Job.Layout",
							.failure = bounds_res.error(),
						}));
					}

					const auto bounds = *bounds_res;
					RegTracker tracker;
					bool has_vft_store = false;
					std::ptrdiff_t fn_ctx_offset = -1;

					std::size_t scan_pos = bounds.begin;
					while (scan_pos < bounds.end && scan_pos + 1 <= executable_code.size())
					{
						const auto* sbytes = reinterpret_cast<const std::uint8_t*>(executable_code.data() + scan_pos);
						ZydisDecodedInstruction sinst;
						ZydisDecodedOperand soperands[ZYDIS_MAX_OPERAND_COUNT];
						if (!ZYAN_SUCCESS(ZydisDecoderDecodeFull(&decoder, sbytes, bounds.end - scan_pos, &sinst, soperands)) || sinst.length == 0)
						{
							scan_pos += 1;
							continue;
						}

						if (win_ops_has_vft_xref(sinst, soperands, code_address + scan_pos, waiting_vft_addr))
						{
							tracker.set_role(soperands[0].reg.value, RegRole::VftAddr);
						}
						else if (sinst.mnemonic == ZYDIS_MNEMONIC_MOV && sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_MEMORY && soperands[1].type == ZYDIS_OPERAND_TYPE_REGISTER &&
							tracker.get_role(soperands[1].reg.value) == RegRole::VftAddr)
						{
							has_vft_store = true;
							tracker.set_role(soperands[0].mem.base, RegRole::This);
						}
						else if (sinst.mnemonic == ZYDIS_MNEMONIC_MOV && sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_REGISTER && soperands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
						{
							const auto src_base_role = tracker.get_role(soperands[1].mem.base);
							if (src_base_role == RegRole::Arg1 || src_base_role == RegRole::Arg2)
							{
								tracker.set_role(soperands[0].reg.value, src_base_role);
							}
						}
						else if (sinst.mnemonic == ZYDIS_MNEMONIC_MOV && sinst.operand_count >= 2 &&
							soperands[0].type == ZYDIS_OPERAND_TYPE_MEMORY && tracker.get_role(soperands[0].mem.base) == RegRole::This &&
							soperands[0].mem.disp.has_displacement && soperands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
						{
							const auto src_role = tracker.get_role(soperands[1].reg.value);
							const auto disp = static_cast<std::ptrdiff_t>(soperands[0].mem.disp.value);
							if (disp >= 0x30 && disp <= 0x400 &&
								(src_role == RegRole::Arg1 || src_role == RegRole::Arg2))
							{
								if (fn_ctx_offset == -1)
								{
									fn_ctx_offset = disp;
								}
							}
						}
						tracker.update(sinst, soperands, code_address + scan_pos);
						scan_pos += sinst.length;
					}

					if (has_vft_store && fn_ctx_offset > 0)
					{
						++matched_calls;
						if (derived_script_ctx != -1 && derived_script_ctx != fn_ctx_offset)
						{
							return std::unexpected(emit(CompatibilityError{
								.capability = "Job.Layout",
								.failure = CompatibilityFailure::ambiguous_evidence,
							}));
						}
						derived_script_ctx = fn_ctx_offset;
					}
		}
		if (matched_calls == 0)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Job.Layout",
				.failure = CompatibilityFailure::missing_signature,
			}));
		}

		if (derived_script_ctx <= 0)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = "Job.Layout",
				.failure = CompatibilityFailure::insufficient_evidence,
			}));
		}
		return JobLayoutEvidence{
			.waiting_scripts_job_script_context_offset = derived_script_ctx,
			.supporting_calls = matched_calls,
			.matched_calls = matched_calls,
		};
	}
}
