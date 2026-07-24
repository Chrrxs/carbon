#pragma once

#include "RobloxModLoader/qt/qobject.hpp"

namespace rml::qt
{
	class QIcon;
	class QMenu;

	class QAction : public QObject
	{
	public:
		enum ActionEvent
		{
			Trigger = 0,
			Hover = 1,
		};

		void setCheckable(bool checkable);
		void setChecked(bool checked);
		[[nodiscard]] bool isChecked() const;
		void setIcon(const QIcon& icon);
		[[nodiscard]] QMenu* menu() const;
		[[nodiscard]] bool queue_trigger();

		[[nodiscard]] static void* activate_address();
	};
}
