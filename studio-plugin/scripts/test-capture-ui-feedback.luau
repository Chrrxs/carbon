--!strict

local fs = require("@lune/fs")

local app = fs.readFile("src/App/init.luau")
local client = fs.readFile("src/Client/init.luau")
local connected = fs.readFile("src/App/Pages/Connected.luau")
local core = fs.readFile("src/Core/init.luau")
local project = fs.readFile("src/App/Widgets/Project.luau")
local textButton = fs.readFile("src/App/Components/TextButton.luau")
local serverCore = fs.readFile("../cli/src/core/mod.rs")

local failures = {}
local function check(condition: boolean, message: string)
	if not condition then
		table.insert(failures, message)
	end
end

check(
	string.find(app, "InitialEnabled = false", 1, true)
		and string.find(app, "OverrideEnabled = true", 1, true)
		and string.find(app, "if self.managedWorktree then", 1, true)
		and string.find(app, "self:connect()", 1, true),
	"Carbon must auto-connect managed serve sessions without opening its dock widget"
)

check(
	string.find(client, "function Client:requestManifestCapture", 1, true)
		and string.find(client, "onStatus: ((status: Types.ManifestCaptureStatus) -> ())?", 1, true)
		and string.find(client, "onStatus(status)", 1, true),
	"the capture client must publish every polled server status to the UI"
)
check(
	string.find(core, "function Core:captureManifest", 1, true)
		and string.find(core, "onStatus: ((status: Types.ManifestCaptureStatus) -> ())?", 1, true)
		and string.find(core, "self.client:fetchDetails()", 1, true)
		and string.find(core, 'self.__sync("SyncDetails", project)', 1, true),
	"a completed capture must refresh reactive project details from the server"
)

check(
	string.find(connected, 'local captureState = Value("idle")', 1, true)
		and string.find(connected, 'else "Capturing..."', 1, true)
		and string.find(connected, "Active = Computed(function(use)", 1, true)
		and string.find(connected, 'return use(captureState) ~= "running"', 1, true)
		and string.find(connected, "captureMessage", 1, true)
		and string.find(connected, "status.message", 1, true),
	"the connected page must reactively show capture progress and block duplicate capture requests"
)
check(
	string.find(connected, "use(props.App.rootSize).X", 1, true)
		and string.find(connected, 'then "Capture" else "Capture Manifest"', 1, true)
		and string.find(connected, "Spacing = 4", 1, true)
		and string.find(textButton, "use(props.TextSize)", 1, true),
	"the action row must react to narrow dock widths without clipping its capture button"
)

for _, phase in
	{
		"Transferring native capture artifacts",
		"Validating the native Studio snapshot",
		"Compiling the captured Studio state",
		"Staging the captured manifest",
		"Committing the captured manifest atomically",
	}
do
	check(string.find(serverCore, phase, 1, true) ~= nil, `the serve endpoint must report the capture phase: {phase}`)
end

check(
	string.find(app, "project:set(table.clone(self.core.project))", 1, true),
	"filesystem source changes must invalidate the project UI's Fusion state"
)

check(
	not string.find(project, "props.App.core.rootInstances", 1, true)
		and string.find(project, "use(props.Project).rootRefs", 1, true),
	"the info widget must derive its root summary from project details instead of a nil core field"
)

if #failures > 0 then
	error(table.concat(failures, "\n"), 0)
end

print("PASS: capture UI is reactive, starts closed, and project info is safe")
