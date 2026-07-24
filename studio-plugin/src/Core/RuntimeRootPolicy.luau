--!strict

local RuntimeRootPolicy = {}

function RuntimeRootPolicy.isPromotable(isService: boolean, isNominalRuntimeRoot: boolean): boolean
	return isService or isNominalRuntimeRoot
end

function RuntimeRootPolicy.shouldPromoteEdit(
	isService: boolean,
	isNominalRuntimeRoot: boolean,
	kind: string,
	isRootEvent: boolean
): boolean
	return RuntimeRootPolicy.isPromotable(isService, isNominalRuntimeRoot)
		and (kind == "Change" or (kind == "Add" and not isRootEvent))
end

function RuntimeRootPolicy.shouldAdoptInitial(
	isService: boolean,
	isNominalRuntimeRoot: boolean,
	existsInSource: boolean
): boolean
	return existsInSource or not RuntimeRootPolicy.isPromotable(isService, isNominalRuntimeRoot)
end

function RuntimeRootPolicy.shouldRefreshHidden(
	isService: boolean,
	isNominalRuntimeRoot: boolean,
	isEditorOnlyRoot: boolean,
	existsInSource: boolean,
	alreadyTracked: boolean,
	isChangedRoot: boolean
): boolean
	return (existsInSource or not isEditorOnlyRoot)
		and (
			RuntimeRootPolicy.shouldAdoptInitial(isService, isNominalRuntimeRoot, existsInSource)
			or alreadyTracked
			-- Native runtime activity alone cannot prove that an otherwise-unmapped
			-- nominal service root became authored. A visible Studio edit promotes
			-- it through shouldPromoteEdit; source-mapped hidden roots remain tracked.
			or (isChangedRoot and not isNominalRuntimeRoot)
		)
end

return RuntimeRootPolicy
