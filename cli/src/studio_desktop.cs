// Carbon's Studio desktop helper. One Windows PowerShell process loads this
// assembly, compiled once and cached by source digest, and runs a complete
// focus or park plan: guard policy changes, virtual desktop routing, window
// activation, and attention clearing. Keep it within C# 5, the language level
// of Windows PowerShell's Add-Type compiler.
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using Microsoft.Win32;

namespace CarbonStudioDesktop
{
	// Interface layout reference: https://github.com/MScholtes/VirtualDesktop
	[ComImport, Guid("372E1D3B-38D3-42E4-A15B-8AB2B178F513"), InterfaceType(ComInterfaceType.InterfaceIsIInspectable)]
	internal interface IApplicationView {}

	[ComImport, Guid("1841C6D7-4F9D-42C0-AF41-8747538F10E5"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
	internal interface IApplicationViewCollection
	{
		int GetViews(out IntPtr array);
		int GetViewsByZOrder(out IntPtr array);
		int GetViewsByAppUserModelId(string id, out IntPtr array);
		[PreserveSig] int GetViewForHwnd(IntPtr hwnd, out IApplicationView view);
	}

	[ComImport, Guid("3F07F4BE-B107-441A-AF0F-39D82529072C"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
	internal interface IVirtualDesktop
	{
		[return: MarshalAs(UnmanagedType.Bool)]
		bool IsViewVisible(IApplicationView view);
		Guid GetId();
	}

	[ComImport, Guid("53F5CA0B-158F-4124-900C-057158060B27"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
	internal interface IVirtualDesktopManagerInternal
	{
		int GetCount();
		void MoveViewToDesktop(IApplicationView view, IVirtualDesktop desktop);
		bool CanViewMoveDesktops(IApplicationView view);
		IVirtualDesktop GetCurrentDesktop();
		void GetDesktops(out IntPtr desktops);
		[PreserveSig] int GetAdjacentDesktop(IVirtualDesktop from, int direction, out IVirtualDesktop desktop);
		void SwitchDesktop(IVirtualDesktop desktop);
		void SwitchDesktopAndMoveForegroundView(IVirtualDesktop desktop);
		IVirtualDesktop CreateDesktop();
		void MoveDesktop(IVirtualDesktop desktop, int index);
		void RemoveDesktop(IVirtualDesktop desktop, IVirtualDesktop fallback);
		IVirtualDesktop FindDesktop(ref Guid desktopId);
	}

	[ComImport, Guid("6D5140C1-7436-11CE-8034-00AA006009FA"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
	internal interface IServiceProvider10
	{
		[return: MarshalAs(UnmanagedType.IUnknown)]
		object QueryService(ref Guid service, ref Guid interfaceId);
	}

	[ComImport, Guid("A5CD92FF-29BE-454C-8D04-D82879FB3F1B"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
	internal interface IVirtualDesktopManager
	{
		[PreserveSig] int IsWindowOnCurrentVirtualDesktop(IntPtr topLevelWindow, out int onCurrentDesktop);
		[PreserveSig] int GetWindowDesktopId(IntPtr topLevelWindow, out Guid desktopId);
		[PreserveSig] int MoveWindowToDesktop(IntPtr topLevelWindow, ref Guid desktopId);
	}

	[ComImport, Guid("AA509086-5CA9-4C25-8F95-589D3C07B48A")]
	internal class VirtualDesktopManager {}

	internal static class Native
	{
		internal const uint ProcessQueryLimitedInformation = 0x00001000;
		internal const uint StillActive = 259;
		internal const uint GwOwner = 4;
		internal const int GwlStyle = -16;
		internal const int GwlExStyle = -20;
		internal const uint WsChild = 0x40000000;
		internal const uint WsExToolWindow = 0x00000080;
		internal const int SwRestore = 9;

		internal delegate bool EnumWindowsCallback(IntPtr window, IntPtr parameter);

		[StructLayout(LayoutKind.Sequential)]
		internal struct FileTime { internal uint Low; internal uint High; }

		[StructLayout(LayoutKind.Sequential)]
		internal struct Rect { internal int Left; internal int Top; internal int Right; internal int Bottom; }

		[StructLayout(LayoutKind.Sequential)]
		internal struct FlashInfo
		{
			internal uint Size;
			internal IntPtr Window;
			internal uint Flags;
			internal uint Count;
			internal uint Timeout;
		}

		[DllImport("kernel32.dll", SetLastError = true)]
		internal static extern IntPtr OpenProcess(uint desiredAccess, bool inheritHandle, uint processId);
		[DllImport("kernel32.dll", SetLastError = true)]
		internal static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);
		[DllImport("kernel32.dll", SetLastError = true)]
		internal static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit, out FileTime kernel, out FileTime user);
		[DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
		internal static extern bool QueryFullProcessImageNameW(IntPtr process, uint flags, StringBuilder path, ref uint size);
		[DllImport("kernel32.dll")]
		internal static extern bool CloseHandle(IntPtr handle);
		[DllImport("kernel32.dll")]
		internal static extern uint GetCurrentThreadId();

		[DllImport("user32.dll")]
		internal static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr parameter);
		[DllImport("user32.dll")]
		internal static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
		[DllImport("user32.dll")]
		internal static extern bool IsWindow(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern bool IsWindowVisible(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern bool IsIconic(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern bool IsHungAppWindow(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern IntPtr GetWindow(IntPtr window, uint command);
		[DllImport("user32.dll", EntryPoint = "GetWindowLongW")]
		internal static extern int GetWindowLong(IntPtr window, int index);
		[DllImport("user32.dll")]
		internal static extern bool GetWindowRect(IntPtr window, out Rect rect);
		[DllImport("user32.dll")]
		internal static extern IntPtr GetLastActivePopup(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern IntPtr GetForegroundWindow();
		[DllImport("user32.dll")]
		internal static extern bool SetForegroundWindow(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern bool ShowWindow(IntPtr window, int command);
		[DllImport("user32.dll")]
		internal static extern bool BringWindowToTop(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern IntPtr SetFocus(IntPtr window);
		[DllImport("user32.dll")]
		internal static extern void SwitchToThisWindow(IntPtr window, bool altTab);
		[DllImport("user32.dll")]
		internal static extern bool AttachThreadInput(uint attach, uint attachTo, bool shouldAttach);
		[DllImport("user32.dll")]
		internal static extern bool FlashWindowEx(ref FlashInfo info);
	}

	internal sealed class Identity
	{
		internal uint ProcessId;
		internal long CreationFileTime;
		internal string Executable;
	}

	internal sealed class Placement
	{
		internal Identity Process;
		internal string DesktopName;
	}

	internal sealed class Plan
	{
		internal string Mode = "";
		internal bool Route;
		internal bool Restore;
		internal Identity Target;
		internal string ParkingDesktop;
		internal readonly List<Placement> Peers = new List<Placement>();
		internal string AudioGuard;
		internal string WindowGuard;
		internal string WindowHook;

		private static string Decode(string value)
		{
			return Encoding.UTF8.GetString(Convert.FromBase64String(value));
		}

		private static Identity ParseIdentity(string[] fields, int start)
		{
			Identity identity = new Identity();
			identity.ProcessId = uint.Parse(fields[start], CultureInfo.InvariantCulture);
			identity.CreationFileTime = long.Parse(fields[start + 1], CultureInfo.InvariantCulture);
			identity.Executable = Decode(fields[start + 2]);
			return identity;
		}

		internal static Plan Parse(string encoded)
		{
			Plan plan = new Plan();
			foreach (string rawLine in Decode(encoded).Split('\n'))
			{
				string line = rawLine.Trim();
				if (line.Length == 0)
				{
					continue;
				}
				string[] fields = line.Split(' ');
				switch (fields[0])
				{
					case "mode": plan.Mode = fields[1]; break;
					case "route": plan.Route = fields[1] == "1"; break;
					case "restore": plan.Restore = fields[1] == "1"; break;
					case "target": plan.Target = ParseIdentity(fields, 1); break;
					case "parking-desktop": plan.ParkingDesktop = Decode(fields[1]); break;
					case "peer":
						Placement placement = new Placement();
						placement.Process = ParseIdentity(fields, 1);
						placement.DesktopName = Decode(fields[4]);
						plan.Peers.Add(placement);
						break;
					case "audio-guard": plan.AudioGuard = Decode(fields[1]); break;
					case "window-guard": plan.WindowGuard = Decode(fields[1]); break;
					case "window-hook": plan.WindowHook = Decode(fields[1]); break;
					default: throw new InvalidOperationException("unknown Studio desktop plan entry '" + fields[0] + "'");
				}
			}
			return plan;
		}
	}

	internal sealed class Json
	{
		private readonly StringBuilder builder = new StringBuilder("{");
		private bool first = true;

		private void Key(string name)
		{
			if (!first)
			{
				builder.Append(',');
			}
			first = false;
			builder.Append(Quote(name)).Append(':');
		}

		internal static string Quote(string value)
		{
			StringBuilder quoted = new StringBuilder("\"");
			foreach (char character in value)
			{
				switch (character)
				{
					case '"': quoted.Append("\\\""); break;
					case '\\': quoted.Append("\\\\"); break;
					case '\n': quoted.Append("\\n"); break;
					case '\r': quoted.Append("\\r"); break;
					case '\t': quoted.Append("\\t"); break;
					default:
						if (character < ' ')
						{
							quoted.Append("\\u").Append(((int)character).ToString("x4", CultureInfo.InvariantCulture));
						}
						else
						{
							quoted.Append(character);
						}
						break;
				}
			}
			return quoted.Append('"').ToString();
		}

		internal Json Number(string name, long value)
		{
			Key(name);
			builder.Append(value.ToString(CultureInfo.InvariantCulture));
			return this;
		}

		internal Json Numbers(string name, IEnumerable<uint> values)
		{
			Key(name);
			builder.Append('[');
			bool firstValue = true;
			foreach (uint value in values)
			{
				if (!firstValue)
				{
					builder.Append(',');
				}
				firstValue = false;
				builder.Append(value.ToString(CultureInfo.InvariantCulture));
			}
			builder.Append(']');
			return this;
		}

		internal Json Strings(string name, IEnumerable<string> values)
		{
			Key(name);
			builder.Append('[');
			bool firstValue = true;
			foreach (string value in values)
			{
				if (!firstValue)
				{
					builder.Append(',');
				}
				firstValue = false;
				builder.Append(Quote(value));
			}
			builder.Append(']');
			return this;
		}

		public override string ToString()
		{
			return builder.ToString() + "}";
		}
	}

	internal sealed class GuardReport
	{
		internal long AudioSessions;
		internal long AudioChanges;
		internal long GuardedThreads;
	}

	internal static class Studio
	{
		private static string NormalizePath(string path)
		{
			try
			{
				return Path.GetFullPath(path).TrimEnd('\\');
			}
			catch
			{
				return path.TrimEnd('\\');
			}
		}

		// Matches the guardians' validation: the exact executable, PID, and
		// creation time of a live process, so a recycled PID is never touched.
		internal static string Validate(Identity identity)
		{
			IntPtr process = Native.OpenProcess(Native.ProcessQueryLimitedInformation, false, identity.ProcessId);
			if (process == IntPtr.Zero)
			{
				return "Roblox Studio process " + identity.ProcessId + " is no longer running";
			}
			try
			{
				uint exitCode;
				if (!Native.GetExitCodeProcess(process, out exitCode) || exitCode != Native.StillActive)
				{
					return "Roblox Studio process " + identity.ProcessId + " is no longer running";
				}
				Native.FileTime creation, exit, kernel, user;
				if (!Native.GetProcessTimes(process, out creation, out exit, out kernel, out user))
				{
					return "could not read the creation time of Roblox Studio process " + identity.ProcessId;
				}
				long actual = unchecked((long)(((ulong)creation.High << 32) | creation.Low));
				if (actual != identity.CreationFileTime)
				{
					return "Roblox Studio process " + identity.ProcessId + " creation time no longer matches";
				}
				uint capacity = 32768;
				StringBuilder path = new StringBuilder((int)capacity);
				if (!Native.QueryFullProcessImageNameW(process, 0, path, ref capacity))
				{
					return "could not read the executable of Roblox Studio process " + identity.ProcessId;
				}
				if (!string.Equals(NormalizePath(path.ToString()), NormalizePath(identity.Executable), StringComparison.OrdinalIgnoreCase))
				{
					return "Roblox Studio process " + identity.ProcessId + " path no longer matches";
				}
				return null;
			}
			finally
			{
				Native.CloseHandle(process);
			}
		}

		internal static void Require(Identity identity)
		{
			string error = Validate(identity);
			if (error != null)
			{
				throw new InvalidOperationException(error);
			}
		}

		private static List<IntPtr> TopLevelWindows(uint processId, bool includeToolWindows)
		{
			List<IntPtr> windows = new List<IntPtr>();
			Native.EnumWindows(delegate(IntPtr window, IntPtr parameter)
			{
				uint windowProcessId;
				Native.GetWindowThreadProcessId(window, out windowProcessId);
				if (windowProcessId != processId || !Native.IsWindowVisible(window))
				{
					return true;
				}
				if (Native.GetWindow(window, Native.GwOwner) != IntPtr.Zero)
				{
					return true;
				}
				if ((unchecked((uint)Native.GetWindowLong(window, Native.GwlStyle)) & Native.WsChild) != 0)
				{
					return true;
				}
				if (!includeToolWindows && (unchecked((uint)Native.GetWindowLong(window, Native.GwlExStyle)) & Native.WsExToolWindow) != 0)
				{
					return true;
				}
				windows.Add(window);
				return true;
			}, IntPtr.Zero);
			return windows;
		}

		private static long Area(IntPtr window)
		{
			Native.Rect rect;
			if (!Native.GetWindowRect(window, out rect))
			{
				return 0;
			}
			return Math.Max(0, rect.Right - rect.Left) * (long)Math.Max(0, rect.Bottom - rect.Top);
		}

		// Studio's main window is its largest visible unowned top-level window,
		// on any virtual desktop. A launching Studio may need a moment to show it.
		internal static IntPtr MainWindow(Identity identity)
		{
			Stopwatch elapsed = Stopwatch.StartNew();
			while (true)
			{
				List<IntPtr> windows = TopLevelWindows(identity.ProcessId, false);
				if (windows.Count == 0)
				{
					windows = TopLevelWindows(identity.ProcessId, true);
				}
				IntPtr best = IntPtr.Zero;
				long bestArea = -1;
				foreach (IntPtr window in windows)
				{
					long area = Area(window);
					if (area > bestArea)
					{
						best = window;
						bestArea = area;
					}
				}
				if (best != IntPtr.Zero)
				{
					return best;
				}
				if (elapsed.ElapsedMilliseconds > 5000)
				{
					throw new InvalidOperationException("Roblox Studio process " + identity.ProcessId + " has no main window");
				}
				Require(identity);
				Thread.Sleep(50);
			}
		}

		internal static List<IntPtr> MainWindows(Identity identity)
		{
			List<IntPtr> windows = TopLevelWindows(identity.ProcessId, false);
			IntPtr main = MainWindow(identity);
			if (!windows.Contains(main))
			{
				windows.Add(main);
			}
			return windows;
		}

		// An active modal dialog owns input, so it is the window to activate.
		internal static IntPtr FocusTarget(IntPtr root, uint processId)
		{
			IntPtr target = root;
			for (int attempt = 0; attempt < 16; attempt++)
			{
				IntPtr popup = Native.GetLastActivePopup(target);
				if (popup == IntPtr.Zero || popup == target || !Native.IsWindowVisible(popup))
				{
					break;
				}
				uint popupProcessId;
				Native.GetWindowThreadProcessId(popup, out popupProcessId);
				if (popupProcessId != processId)
				{
					break;
				}
				target = popup;
			}
			return target;
		}

		internal static uint WindowProcess(IntPtr window)
		{
			if (window == IntPtr.Zero)
			{
				return 0;
			}
			uint processId;
			Native.GetWindowThreadProcessId(window, out processId);
			return processId;
		}

		internal static int StopFlashing(uint processId)
		{
			int windows = 0;
			Native.EnumWindows(delegate(IntPtr window, IntPtr parameter)
			{
				uint windowProcessId;
				Native.GetWindowThreadProcessId(window, out windowProcessId);
				if (windowProcessId != processId)
				{
					return true;
				}
				Native.FlashInfo info = new Native.FlashInfo();
				info.Size = (uint)Marshal.SizeOf(typeof(Native.FlashInfo));
				info.Window = window;
				Native.FlashWindowEx(ref info);
				windows++;
				return true;
			}, IntPtr.Zero);
			return windows;
		}
	}

	internal static class Desktops
	{
		private const int TypeElementNotFound = unchecked((int)0x8002802B);
		private static readonly Guid ImmersiveShell = new Guid("C2F03A33-21F5-47FA-B4BB-156362A2F239");
		private static readonly Guid ManagerService = new Guid("C5E0CDCA-7B6E-41B2-9FC4-D93975CC467B");
		private const string DesktopsKey = @"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops\Desktops";

		internal static void RequireSupport()
		{
			if (Environment.OSVersion.Version.Build < 26100)
			{
				throw new InvalidOperationException("automatic Studio desktop routing requires Windows 11 24H2 or newer");
			}
		}

		private static void Services(out IVirtualDesktopManagerInternal manager, out IApplicationViewCollection views)
		{
			IServiceProvider10 shell = (IServiceProvider10)Activator.CreateInstance(Type.GetTypeFromCLSID(ImmersiveShell));
			Guid managerService = ManagerService;
			Guid managerInterface = typeof(IVirtualDesktopManagerInternal).GUID;
			Guid viewsInterface = typeof(IApplicationViewCollection).GUID;
			manager = (IVirtualDesktopManagerInternal)shell.QueryService(ref managerService, ref managerInterface);
			views = (IApplicationViewCollection)shell.QueryService(ref viewsInterface, ref viewsInterface);
		}

		internal static Guid Current()
		{
			IVirtualDesktopManagerInternal manager;
			IApplicationViewCollection views;
			Services(out manager, out views);
			IVirtualDesktop desktop = manager.GetCurrentDesktop();
			if (desktop == null)
			{
				throw new InvalidOperationException("Windows did not report the active virtual desktop");
			}
			return desktop.GetId();
		}

		internal static Guid Resolve(string name)
		{
			List<Guid> matches = new List<Guid>();
			using (RegistryKey desktops = Registry.CurrentUser.OpenSubKey(DesktopsKey))
			{
				if (desktops != null)
				{
					foreach (string id in desktops.GetSubKeyNames())
					{
						using (RegistryKey desktop = desktops.OpenSubKey(id))
						{
							string desktopName = desktop == null ? null : desktop.GetValue("Name") as string;
							Guid parsed;
							if (!string.IsNullOrEmpty(desktopName)
								&& string.Equals(desktopName, name, StringComparison.OrdinalIgnoreCase)
								&& Guid.TryParse(id, out parsed))
							{
								matches.Add(parsed);
							}
						}
					}
				}
			}
			if (matches.Count == 0)
			{
				throw new InvalidOperationException("Windows virtual desktop '" + name + "' was not found");
			}
			if (matches.Count > 1)
			{
				throw new InvalidOperationException("Windows virtual desktop name '" + name + "' is ambiguous");
			}
			return matches[0];
		}

		// Returns null when Windows has no application view for the window.
		private static Guid? WindowDesktop(IntPtr window)
		{
			IVirtualDesktopManager manager = (IVirtualDesktopManager)new VirtualDesktopManager();
			Guid desktopId;
			int result = manager.GetWindowDesktopId(window, out desktopId);
			if (result == TypeElementNotFound)
			{
				return null;
			}
			if (result != 0)
			{
				Marshal.ThrowExceptionForHR(result);
			}
			return desktopId;
		}

		// Moves a window's application view and waits until Windows reports it on
		// the requested desktop. A window without a view is left where it is.
		internal static bool Move(IntPtr window, Guid desktopId)
		{
			Guid? current = WindowDesktop(window);
			if (current == null)
			{
				return false;
			}
			if (current.Value == desktopId)
			{
				return true;
			}
			IVirtualDesktopManagerInternal manager;
			IApplicationViewCollection views;
			Services(out manager, out views);
			IApplicationView view;
			int result = views.GetViewForHwnd(window, out view);
			if (result == TypeElementNotFound)
			{
				return false;
			}
			if (result != 0)
			{
				Marshal.ThrowExceptionForHR(result);
			}
			IVirtualDesktop desktop = manager.FindDesktop(ref desktopId);
			if (desktop == null)
			{
				throw new InvalidOperationException("Windows virtual desktop no longer exists");
			}
			manager.MoveViewToDesktop(view, desktop);
			for (int attempt = 0; attempt < 40; attempt++)
			{
				current = WindowDesktop(window);
				if (current != null && current.Value == desktopId)
				{
					return true;
				}
				Thread.Sleep(10);
			}
			throw new InvalidOperationException("Windows did not move Roblox Studio to the requested virtual desktop");
		}
	}

	internal static class Guards
	{
		private static readonly UTF8Encoding Utf8 = new UTF8Encoding(false);

		private static string Request(string pipeName, string policy, int connectTimeout)
		{
			using (NamedPipeClientStream pipe = new NamedPipeClientStream(".", pipeName, PipeDirection.InOut, PipeOptions.Asynchronous))
			{
				pipe.Connect(connectTimeout);
				using (StreamWriter writer = new StreamWriter(pipe, Utf8, 1024, true))
				using (StreamReader reader = new StreamReader(pipe, Utf8, false, 1024, true))
				{
					writer.AutoFlush = true;
					writer.WriteLine(policy);
					var response = reader.ReadLineAsync();
					if (!response.Wait(10000) || string.IsNullOrEmpty(response.Result))
					{
						throw new InvalidOperationException("the guardian returned no policy acknowledgement");
					}
					return response.Result;
				}
			}
		}

		private static void Spawn(string apartment, string script, Identity identity, string policy, string extra)
		{
			ProcessStartInfo start = new ProcessStartInfo();
			start.FileName = Path.Combine(Environment.SystemDirectory, @"WindowsPowerShell\v1.0\powershell.exe");
			start.Arguments = apartment + " -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"" + script +
				"\" -Mode spawn -TargetProcessId " + identity.ProcessId.ToString(CultureInfo.InvariantCulture) +
				" -ExecutableBase64 " + Convert.ToBase64String(Encoding.UTF8.GetBytes(identity.Executable)) +
				" -CreationFileTime " + identity.CreationFileTime.ToString(CultureInfo.InvariantCulture) +
				" -Policy " + policy + extra + " -ConnectTimeoutMilliseconds 10000";
			start.UseShellExecute = false;
			start.CreateNoWindow = true;
			start.RedirectStandardError = true;
			start.RedirectStandardOutput = true;
			using (Process process = Process.Start(start))
			{
				string error = process.StandardError.ReadToEnd();
				process.StandardOutput.ReadToEnd();
				if (!process.WaitForExit(30000) || process.ExitCode != 0)
				{
					throw new InvalidOperationException("could not start the guardian: " + error.Trim());
				}
			}
		}

		// Ask the persistent guardian for a policy, starting it when it is not
		// running yet. The guardians outlive this process and keep enforcing it.
		private static string Command(string pipeName, string apartment, string script, Identity identity, string policy, string extra)
		{
			try
			{
				return Request(pipeName, policy, 250);
			}
			catch (TimeoutException)
			{
			}
			catch (IOException)
			{
			}
			Spawn(apartment, script, identity, policy, extra);
			return Request(pipeName, policy, 10000);
		}

		private static long Field(string report, string name)
		{
			Match match = Regex.Match(report, "\"" + name + "\":(\\d+)");
			if (!match.Success)
			{
				throw new InvalidOperationException("the guardian report has no " + name);
			}
			return long.Parse(match.Groups[1].Value, CultureInfo.InvariantCulture);
		}

		private static void RequirePolicy(string report, string policy)
		{
			Match match = Regex.Match(report, "\"policy\":\"([a-z]+)\"");
			if (!match.Success || match.Groups[1].Value != policy)
			{
				throw new InvalidOperationException("the guardian acknowledged an unexpected policy: " + report);
			}
		}

		internal static GuardReport Apply(Plan plan, Identity identity, bool parked)
		{
			GuardReport result = new GuardReport();
			string audioPolicy = parked ? "muted" : "audible";
			string audio = Command(
				"carbon-studio-audio-v4-" + identity.ProcessId + "-" + identity.CreationFileTime,
				"-Mta",
				plan.AudioGuard,
				identity,
				audioPolicy,
				"");
			RequirePolicy(audio, audioPolicy);
			if (Field(audio, "failed_sessions") != 0 || Field(audio, "remaining_mismatched_sessions") != 0)
			{
				throw new InvalidOperationException("the audio guardian left Studio sessions outside the " + audioPolicy + " policy: " + audio);
			}
			result.AudioSessions = Field(audio, "matched_sessions");
			result.AudioChanges = Field(audio, "changed_sessions");

			string windowPolicy = parked ? "parked" : "active";
			string window = Command(
				"carbon-studio-window-v1-" + identity.ProcessId + "-" + identity.CreationFileTime,
				"-Sta",
				plan.WindowGuard,
				identity,
				windowPolicy,
				" -HookLibrary \"" + plan.WindowHook + "\"");
			RequirePolicy(window, windowPolicy);
			result.GuardedThreads = Field(window, "guarded_threads");
			if (!parked && result.GuardedThreads != 0)
			{
				throw new InvalidOperationException("the window guardian left " + result.GuardedThreads + " Studio UI thread(s) guarded");
			}
			return result;
		}
	}

	internal static class Activation
	{
		private const int TimeoutMilliseconds = 2000;

		private static void Request(IntPtr target, bool switchWindow)
		{
			uint current = Native.GetCurrentThreadId();
			IntPtr foreground = Native.GetForegroundWindow();
			uint ignored;
			uint foregroundThread = foreground == IntPtr.Zero ? 0 : Native.GetWindowThreadProcessId(foreground, out ignored);
			uint targetThread = Native.GetWindowThreadProcessId(target, out ignored);
			bool attachedForeground = false;
			bool attachedTarget = false;
			try
			{
				// Sharing input state with the foreground thread lets this process
				// pass Windows' foreground lock without synthesizing input.
				if (foregroundThread != 0 && foregroundThread != current)
				{
					attachedForeground = Native.AttachThreadInput(current, foregroundThread, true);
				}
				if (targetThread != 0 && targetThread != current && targetThread != foregroundThread)
				{
					attachedTarget = Native.AttachThreadInput(current, targetThread, true);
				}
				Native.BringWindowToTop(target);
				Native.SetForegroundWindow(target);
				Native.SetFocus(target);
			}
			finally
			{
				if (attachedTarget)
				{
					Native.AttachThreadInput(current, targetThread, false);
				}
				if (attachedForeground)
				{
					Native.AttachThreadInput(current, foregroundThread, false);
				}
			}
			if (switchWindow && Studio.WindowProcess(Native.GetForegroundWindow()) != Studio.WindowProcess(target))
			{
				Native.SwitchToThisWindow(target, true);
			}
		}

		// Studio is in front once any of its windows is the foreground window: Qt
		// may hand activation to its own active dialog or tool window.
		internal static void Activate(IntPtr target, string description)
		{
			uint processId = Studio.WindowProcess(target);
			if (Native.IsIconic(target))
			{
				Native.ShowWindow(target, Native.SwRestore);
			}
			Stopwatch elapsed = Stopwatch.StartNew();
			for (int attempt = 0; ; attempt++)
			{
				if (Studio.WindowProcess(Native.GetForegroundWindow()) == processId)
				{
					return;
				}
				if (!Native.IsWindow(target))
				{
					throw new InvalidOperationException(description + " closed before it could be activated");
				}
				Request(target, attempt > 0);
				for (int poll = 0; poll < 10; poll++)
				{
					if (Studio.WindowProcess(Native.GetForegroundWindow()) == processId)
					{
						return;
					}
					Thread.Sleep(10);
				}
				if (elapsed.ElapsedMilliseconds >= TimeoutMilliseconds)
				{
					uint foregroundProcess = Studio.WindowProcess(Native.GetForegroundWindow());
					string reason = Native.IsHungAppWindow(target)
						? description + " is not responding"
						: "Windows kept " + (foregroundProcess == 0 ? "no window" : "process " + foregroundProcess) + " in the foreground";
					throw new InvalidOperationException(description + " rejected foreground activation: " + reason);
				}
			}
		}
	}

	public static class Helper
	{
		public static string Run(string encodedPlan)
		{
			Plan plan = Plan.Parse(encodedPlan);
			switch (plan.Mode)
			{
				case "probe": return new Json().Number("protocol", 1).ToString();
				case "focus": return Focus(plan);
				case "park": return Park(plan);
				default: throw new InvalidOperationException("unknown Studio desktop helper mode '" + plan.Mode + "'");
			}
		}

		private static string Focus(Plan plan)
		{
			Identity target = plan.Target;
			Studio.Require(target);
			List<string> warnings = new List<string>();
			List<uint> parkedProcessIds = new List<uint>();
			List<Identity> parkedProcesses = new List<Identity>();
			long attentionWindows = 0;
			long postFocusAttentionWindows = 0;
			GuardReport targetGuard = new GuardReport();
			long peerSessions = 0;
			long peerChanges = 0;
			long peerThreads = 0;

			if (plan.Route)
			{
				try
				{
					Guards.Apply(plan, target, true);
				}
				catch (Exception error)
				{
					throw new InvalidOperationException("failed to mute the selected Studio before routing desktops: " + error.Message, error);
				}
				List<Placement> guardedPeers = new List<Placement>();
				foreach (Placement peer in plan.Peers)
				{
					if (peer.Process.ProcessId == target.ProcessId)
					{
						warnings.Add("Studio PID " + peer.Process.ProcessId + " was not parked because it is also the focus target");
						continue;
					}
					try
					{
						GuardReport guard = Guards.Apply(plan, peer.Process, true);
						peerSessions += guard.AudioSessions;
						peerChanges += guard.AudioChanges;
						peerThreads += guard.GuardedThreads;
					}
					catch (Exception error)
					{
						throw new InvalidOperationException(
							"failed to mute sibling Studio PID " + peer.Process.ProcessId +
							"; already guarded Studios remain parked: " + error.Message, error);
					}
					guardedPeers.Add(peer);
				}

				try
				{
					Desktops.RequireSupport();
					Guid active = Desktops.Current();
					IntPtr main = Studio.MainWindow(target);
					if (!Desktops.Move(main, active))
					{
						throw new InvalidOperationException("Roblox Studio's main window has no movable application view");
					}
					foreach (IntPtr window in Studio.MainWindows(target))
					{
						if (window != main)
						{
							try
							{
								Desktops.Move(window, active);
							}
							catch (Exception)
							{
								// Secondary windows follow when Windows lets them; the main
								// window, which is activated below, is already in place.
							}
						}
					}
				}
				catch (Exception error)
				{
					throw new InvalidOperationException("failed to route Studio desktops; all selected Studios remain parked and muted: " + error.Message, error);
				}

				foreach (Placement peer in guardedPeers)
				{
					try
					{
						Studio.Require(peer.Process);
						Guid parking = Desktops.Resolve(peer.DesktopName);
						IntPtr window = Studio.MainWindow(peer.Process);
						if (!Desktops.Move(window, parking))
						{
							throw new InvalidOperationException("its main window has no movable application view");
						}
						attentionWindows += Studio.StopFlashing(peer.Process.ProcessId);
						parkedProcessIds.Add(peer.Process.ProcessId);
						parkedProcesses.Add(peer.Process);
					}
					catch (Exception error)
					{
						warnings.Add("Studio PID " + peer.Process.ProcessId + " was not parked on desktop '" + peer.DesktopName + "': " + error.Message);
					}
				}

				try
				{
					targetGuard = Guards.Apply(plan, target, false);
				}
				catch (Exception error)
				{
					throw new InvalidOperationException("failed to restore the focused Studio's audio and activation: " + error.Message, error);
				}
			}

			IntPtr previous = Native.GetForegroundWindow();
			Studio.Require(target);
			IntPtr root = Studio.MainWindow(target);
			Activation.Activate(Studio.FocusTarget(root, target.ProcessId), "Roblox Studio");

			// Windows can relatch a shared taskbar group while activating Studio.
			foreach (Identity parked in parkedProcesses)
			{
				string error = Studio.Validate(parked);
				if (error == null)
				{
					postFocusAttentionWindows += Studio.StopFlashing(parked.ProcessId);
				}
				else
				{
					warnings.Add("Studio PID " + parked.ProcessId + " attention was not cleared: " + error);
				}
			}

			if (plan.Restore && previous != IntPtr.Zero && Native.IsWindow(previous)
				&& Studio.WindowProcess(previous) != target.ProcessId)
			{
				try
				{
					Activation.Activate(previous, "the previously focused window");
				}
				catch (Exception error)
				{
					throw new InvalidOperationException("Windows denied restoration of the previously focused window: " + error.Message, error);
				}
			}

			return new Json()
				.Number("parked", parkedProcessIds.Count)
				.Numbers("parked_process_ids", parkedProcessIds)
				.Number("attention_windows", attentionWindows)
				.Number("post_focus_attention_windows", postFocusAttentionWindows)
				.Number("target_audio_sessions", targetGuard.AudioSessions)
				.Number("target_audio_changes", targetGuard.AudioChanges)
				.Number("peer_audio_sessions", peerSessions)
				.Number("peer_audio_changes", peerChanges)
				.Number("peer_guarded_threads", peerThreads)
				.Strings("warnings", warnings)
				.ToString();
		}

		private static string Park(Plan plan)
		{
			Identity target = plan.Target;
			Studio.Require(target);
			GuardReport guard;
			try
			{
				guard = Guards.Apply(plan, target, true);
			}
			catch (Exception error)
			{
				throw new InvalidOperationException("failed to guard the parked Studio: " + error.Message, error);
			}
			int attentionWindows;
			try
			{
				Desktops.RequireSupport();
				Guid parking = Desktops.Resolve(plan.ParkingDesktop);
				IntPtr window = Studio.MainWindow(target);
				if (!Desktops.Move(window, parking))
				{
					throw new InvalidOperationException("its main window has no movable application view");
				}
				attentionWindows = Studio.StopFlashing(target.ProcessId);
			}
			catch (Exception error)
			{
				throw new InvalidOperationException(
					"failed to move Studio to desktop '" + plan.ParkingDesktop + "'; it remains muted and guarded for retry: " + error.Message, error);
			}
			return new Json()
				.Number("attention_windows", attentionWindows)
				.Number("audio_sessions", guard.AudioSessions)
				.Number("audio_changes", guard.AudioChanges)
				.Number("guarded_threads", guard.GuardedThreads)
				.ToString();
		}
	}
}
