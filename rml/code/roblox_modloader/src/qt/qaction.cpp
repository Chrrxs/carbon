#include "RobloxModLoader/qt/qaction.hpp"

#include "RobloxModLoader/qt/qicon.hpp"
#include "RobloxModLoader/qt/qmenu.hpp"
#include "RobloxModLoader/qt/qt_module.hpp"

namespace rml::qt
{
	void QAction::setCheckable(const bool checkable)
	{
		static const auto fn = detail::widgets<void (*)(void*, bool)>("?setCheckable@QAction@@QEAAX_N@Z");
		if (fn)
			fn(this, checkable);
	}

	void QAction::setChecked(const bool checked)
	{
		static const auto fn = detail::widgets<void (*)(void*, bool)>("?setChecked@QAction@@QEAAX_N@Z");
		if (fn)
			fn(this, checked);
	}

	bool QAction::isChecked() const
	{
		static const auto fn = detail::widgets<bool (*)(const void*)>("?isChecked@QAction@@QEBA_NXZ");
		return fn && fn(this);
	}

	void QAction::setIcon(const QIcon& icon)
	{
		static const auto fn = detail::widgets<void (*)(void*, const void*)>("?setIcon@QAction@@QEAAXAEBVQIcon@@@Z");
		if (fn)
			fn(this, icon.data());
	}

	QMenu* QAction::menu() const
	{
		static const auto fn = detail::widgets<void* (*)(const void*)>("?menu@QAction@@QEBAPEAVQMenu@@XZ");
		return fn ? static_cast<QMenu*>(fn(this)) : nullptr;
	}

	bool QAction::queue_trigger()
	{
		// QMetaObject::invokeMethod with Qt::QueuedConnection is thread-safe and
		// keeps the Studio action on its owning UI thread. QGenericArgument is two
		// pointers in Qt 5; every argument is empty for QAction::trigger().
		struct QGenericArgument
		{
			const char* name{};
			const void* data{};
		};
		using Invoke = bool (*)(
		    void*, const char*, int,
		    QGenericArgument, QGenericArgument, QGenericArgument, QGenericArgument, QGenericArgument,
		    QGenericArgument, QGenericArgument, QGenericArgument, QGenericArgument, QGenericArgument);
		static const auto fn = detail::core<Invoke>(
		    "?invokeMethod@QMetaObject@@SA_NPEAVQObject@@PEBDW4ConnectionType@Qt@@VQGenericArgument@@333333333@Z");
		const QGenericArgument empty{};
		return fn && fn(this, "trigger", 2, empty, empty, empty, empty, empty, empty, empty, empty, empty, empty);
	}

	void* QAction::activate_address()
	{
		return detail::widgets_export("?activate@QAction@@QEAAXW4ActionEvent@1@@Z");
	}
}
