--!strict

local RequestPolicy = require("../src/Client/RequestPolicy")

assert(RequestPolicy.sourcePageInstances(false) == 4096, "full property pages must retain their bounded size")
assert(
	RequestPolicy.sourcePageInstances(true) == 32768,
	"exact-build metadata pages must stay below Studio's request-count budget"
)
assert(
	math.ceil(277_160 / RequestPolicy.sourcePageInstances(true)) <= 9,
	"the Strain acceptance fixture must need at most nine metadata requests"
)
assert(RequestPolicy.retryDelay("unrelated error", 0) == nil, "permanent errors must not be retried")
assert(RequestPolicy.retryDelay("Number of requests exceeded limit", 0) == 1, "rate limits must back off")
assert(RequestPolicy.retryDelay("Number of requests exceeded limit", 10) == 30, "backoff must be capped")

print("PASS: Studio request policy stays within the shared HttpService budget")
