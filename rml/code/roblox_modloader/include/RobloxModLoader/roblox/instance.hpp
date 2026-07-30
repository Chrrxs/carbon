#pragma once
#include "object.hpp"
#include "reflection/object.hpp"


#include <memory>
#include <vector>

namespace RBX
{
	using namespace Reflection;

	class Instance;

	using Instances = std::vector<std::shared_ptr<Instance>>;

	class Instance : public Object
	{
	public:
		[[nodiscard]] Instance* get_parent() const;
		[[nodiscard]] Instances* get_children() const;
		[[nodiscard]] std::string_view get_name() const;

		template<typename T = Instance>
		T* as()
		{
			return static_cast<T*>(this);
		}

		std::string get_full_name();
	};
}
