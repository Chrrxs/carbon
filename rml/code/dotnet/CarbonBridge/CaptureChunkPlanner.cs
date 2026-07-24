namespace Carbon.RmlBridge;

internal readonly record struct CaptureChunkLayout(
    uint[] RootOrdinals,
    uint[] FrontierOrdinals,
    uint[] MemberOrdinals,
    uint[] DependencyOrdinals,
    uint NodeCount);

internal readonly record struct CaptureReferenceDependency(
    uint OwnerOrdinal,
    uint TargetOrdinal);

internal static class CaptureChunkPlanner
{
    // Keeps each non-cancellable engine serializer invocation short enough to
    // preserve Studio responsiveness while still amortizing per-call overhead.
    internal const uint DefaultNodeBudget = 32_768;

    internal static CaptureChunkLayout[] Plan(
        IReadOnlyList<CaptureHierarchyNode> nodes,
        IReadOnlyList<uint> directRootOrdinals,
        uint nodeBudget = DefaultNodeBudget,
        IReadOnlyList<CaptureReferenceDependency>? referenceDependencies = null)
    {
        ArgumentNullException.ThrowIfNull(nodes);
        ArgumentNullException.ThrowIfNull(directRootOrdinals);
        if (nodes.Count == 0 || nodeBudget == 0)
        {
            throw new ArgumentOutOfRangeException(nameof(nodeBudget));
        }

        var serviceShells = new bool[nodes.Count];
        ulong expectedNodeCount = 0;
        for (var ordinal = 1; ordinal < nodes.Count; ordinal++)
        {
            var parent = nodes[ordinal].ParentOrdinal;
            if (parent >= ordinal)
            {
                throw new InvalidDataException(
                    "capture chunk hierarchy parent does not precede its child");
            }
            serviceShells[ordinal] = parent == 0;
            if (!serviceShells[ordinal])
            {
                expectedNodeCount++;
            }
        }

        var currentWave = directRootOrdinals.ToArray();
        var selectedDirectRoots = new bool[nodes.Count];
        foreach (var root in currentWave)
        {
            if (root == 0 || root >= nodes.Count || serviceShells[checked((int)root)]
                || !serviceShells[checked((int)nodes[checked((int)root)].ParentOrdinal)]
                || selectedDirectRoots[checked((int)root)])
            {
                throw new InvalidDataException(
                    "capture chunk direct root is not a unique service child");
            }
            selectedDirectRoots[checked((int)root)] = true;
        }

        // Flat service children are the common scale fixture and need no
        // frontier graph at all. Packing this case directly avoids five
        // million-node-sized planner arrays while preserving the same chunks.
        if (checked((ulong)currentWave.Length) == expectedNodeCount)
        {
            var flat = new List<CaptureChunkLayout>(
                checked((currentWave.Length + (int)nodeBudget - 1) / (int)nodeBudget));
            for (var start = 0; start < currentWave.Length; start += checked((int)nodeBudget))
            {
                var count = Math.Min(checked((int)nodeBudget), currentWave.Length - start);
                var members = currentWave.AsSpan(start, count).ToArray();
                flat.Add(new(
                    members,
                    [],
                    members,
                    [],
                    checked((uint)count)));
            }
            return AttachReferenceDependencies(
                nodes,
                flat.ToArray(),
                referenceDependencies ?? []);
        }

        var childCounts = new int[nodes.Count];
        for (var ordinal = 1; ordinal < nodes.Count; ordinal++)
        {
            childCounts[checked((int)nodes[ordinal].ParentOrdinal)]++;
        }
        var childOffsets = new int[nodes.Count];
        var edgeCount = 0;
        for (var ordinal = 0; ordinal < nodes.Count; ordinal++)
        {
            childOffsets[ordinal] = edgeCount;
            edgeCount = checked(edgeCount + childCounts[ordinal]);
        }
        var childCursor = (int[])childOffsets.Clone();
        var children = new uint[edgeCount];
        for (var ordinal = 1; ordinal < nodes.Count; ordinal++)
        {
            var parent = checked((int)nodes[ordinal].ParentOrdinal);
            children[childCursor[parent]++] = checked((uint)ordinal);
        }

        var result = new List<CaptureChunkLayout>();
        var visited = new bool[nodes.Count];
        ulong visitedNodeCount = 0;
        while (currentWave.Length != 0)
        {
            var nextWave = new List<uint>();
            var chunkRoots = new List<uint>();
            var chunkFrontier = new List<uint>();
            var chunkMembers = new List<uint>();
            var componentMembers = new List<uint>();
            var stack = new List<uint>();
            uint chunkNodeCount = 0;
            void FlushChunk()
            {
                if (chunkRoots.Count == 0)
                {
                    return;
                }
                result.Add(new(
                    chunkRoots.ToArray(),
                    chunkFrontier.ToArray(),
                    chunkMembers.ToArray(),
                    [],
                    chunkNodeCount));
                chunkRoots.Clear();
                chunkFrontier.Clear();
                chunkMembers.Clear();
                chunkNodeCount = 0;
            }
            foreach (var root in currentWave)
            {
                stack.Clear();
                componentMembers.Clear();
                stack.Add(root);
                uint componentNodeCount = 0;
                while (stack.Count != 0 && componentNodeCount < nodeBudget)
                {
                    var last = stack.Count - 1;
                    var ordinal = stack[last];
                    stack.RemoveAt(last);
                    var index = checked((int)ordinal);
                    if (visited[index])
                    {
                        throw new InvalidDataException(
                            "capture chunk planner encountered a duplicate hierarchy node");
                    }
                    visited[index] = true;
                    componentMembers.Add(ordinal);
                    componentNodeCount++;
                    visitedNodeCount++;
                    var start = childOffsets[index];
                    var end = start + childCounts[index];
                    for (var child = end - 1; child >= start; child--)
                    {
                        stack.Add(children[child]);
                    }
                }

                var frontier = stack.Count == 0
                    ? Array.Empty<uint>()
                    : new uint[stack.Count];
                for (var index = 0; stack.Count != 0; index++)
                {
                    var last = stack.Count - 1;
                    frontier[index] = stack[last];
                    stack.RemoveAt(last);
                }
                nextWave.AddRange(frontier);
                if (chunkRoots.Count == 0)
                {
                    chunkRoots.Add(root);
                    chunkFrontier.AddRange(frontier);
                    chunkMembers.AddRange(componentMembers);
                    chunkNodeCount = componentNodeCount;
                    continue;
                }
                if ((ulong)chunkNodeCount + componentNodeCount > nodeBudget)
                {
                    FlushChunk();
                }
                chunkRoots.Add(root);
                chunkFrontier.AddRange(frontier);
                chunkMembers.AddRange(componentMembers);
                chunkNodeCount = checked(chunkNodeCount + componentNodeCount);
            }
            FlushChunk();
            currentWave = nextWave.ToArray();
        }

        if (visitedNodeCount != expectedNodeCount)
        {
            throw new InvalidDataException(
                $"capture chunk plan covers {visitedNodeCount} of {expectedNodeCount} serialized nodes");
        }
        return AttachReferenceDependencies(
            nodes,
            result.ToArray(),
            referenceDependencies ?? []);
    }

    private static CaptureChunkLayout[] AttachReferenceDependencies(
        IReadOnlyList<CaptureHierarchyNode> nodes,
        CaptureChunkLayout[] chunks,
        IReadOnlyList<CaptureReferenceDependency> dependencies)
    {
        if (dependencies.Count == 0)
        {
            return chunks;
        }

        chunks = IsolateMaskedDependencyOwners(nodes, chunks, dependencies);
        var chunkByOrdinal = IndexMemberChunks(nodes, chunks);

        var targetsByChunk = Enumerable.Range(0, chunks.Length)
            .Select(_ => new SortedSet<uint>())
            .ToArray();
        foreach (var dependency in dependencies)
        {
            if (dependency.OwnerOrdinal == 0
                || dependency.OwnerOrdinal >= nodes.Count
                || dependency.TargetOrdinal == 0
                || dependency.TargetOrdinal >= nodes.Count)
            {
                throw new InvalidDataException(
                    "capture reference dependency ordinal is invalid");
            }
            var ownerChunk = chunkByOrdinal[checked((int)dependency.OwnerOrdinal)];
            var targetChunk = chunkByOrdinal[checked((int)dependency.TargetOrdinal)];
            if (ownerChunk < 0 || targetChunk < 0)
            {
                throw new InvalidDataException(
                    "capture reference dependency is outside serialized hierarchy pages");
            }
            if (ownerChunk != targetChunk)
            {
                targetsByChunk[ownerChunk].Add(dependency.TargetOrdinal);
            }
        }

        for (var chunkIndex = 0; chunkIndex < chunks.Length; chunkIndex++)
        {
            chunks[chunkIndex] = chunks[chunkIndex] with
            {
                DependencyOrdinals = targetsByChunk[chunkIndex].ToArray(),
            };
        }
        return chunks;
    }

    private static int[] IndexMemberChunks(
        IReadOnlyList<CaptureHierarchyNode> nodes,
        IReadOnlyList<CaptureChunkLayout> chunks)
    {
        var chunkByOrdinal = new int[nodes.Count];
        Array.Fill(chunkByOrdinal, -1);
        for (var chunkIndex = 0; chunkIndex < chunks.Count; chunkIndex++)
        {
            foreach (var ordinal in chunks[chunkIndex].MemberOrdinals)
            {
                if (ordinal == 0 || ordinal >= nodes.Count
                    || chunkByOrdinal[checked((int)ordinal)] >= 0)
                {
                    throw new InvalidDataException(
                        "capture chunk dependency plan has invalid member coverage");
                }
                chunkByOrdinal[checked((int)ordinal)] = chunkIndex;
            }
        }
        return chunkByOrdinal;
    }

    private static CaptureChunkLayout[] IsolateMaskedDependencyOwners(
        IReadOnlyList<CaptureHierarchyNode> nodes,
        CaptureChunkLayout[] chunks,
        IReadOnlyList<CaptureReferenceDependency> dependencies)
    {
        var childCounts = new int[nodes.Count];
        for (var ordinal = 1; ordinal < nodes.Count; ordinal++)
        {
            childCounts[checked((int)nodes[ordinal].ParentOrdinal)]++;
        }
        var childOffsets = new int[nodes.Count];
        var edgeCount = 0;
        for (var ordinal = 0; ordinal < nodes.Count; ordinal++)
        {
            childOffsets[ordinal] = edgeCount;
            edgeCount = checked(edgeCount + childCounts[ordinal]);
        }
        var childCursor = (int[])childOffsets.Clone();
        var children = new uint[edgeCount];
        for (var ordinal = 1; ordinal < nodes.Count; ordinal++)
        {
            var parent = checked((int)nodes[ordinal].ParentOrdinal);
            children[childCursor[parent]++] = checked((uint)ordinal);
        }

        var result = chunks.ToList();
        var isolatedOwners = new HashSet<uint>();
        while (true)
        {
            var chunkByOrdinal = IndexMemberChunks(nodes, result);
            CaptureReferenceDependency? collision = null;
            var ownerChunk = -1;
            foreach (var dependency in dependencies
                .Distinct()
                .OrderBy(dependency => dependency.OwnerOrdinal)
                .ThenBy(dependency => dependency.TargetOrdinal))
            {
                if (dependency.OwnerOrdinal == 0
                    || dependency.OwnerOrdinal >= nodes.Count
                    || dependency.TargetOrdinal == 0
                    || dependency.TargetOrdinal >= nodes.Count)
                {
                    throw new InvalidDataException(
                        "capture reference dependency ordinal is invalid");
                }
                var candidateOwnerChunk =
                    chunkByOrdinal[checked((int)dependency.OwnerOrdinal)];
                var targetChunk =
                    chunkByOrdinal[checked((int)dependency.TargetOrdinal)];
                if (candidateOwnerChunk < 0 || targetChunk < 0)
                {
                    throw new InvalidDataException(
                        "capture reference dependency is outside serialized hierarchy pages");
                }
                if (candidateOwnerChunk != targetChunk
                    && result[candidateOwnerChunk].FrontierOrdinals
                        .Contains(dependency.TargetOrdinal))
                {
                    collision = dependency;
                    ownerChunk = candidateOwnerChunk;
                    break;
                }
            }
            if (collision is null)
            {
                return result.ToArray();
            }

            var owner = collision.Value.OwnerOrdinal;
            if (!isolatedOwners.Add(owner))
            {
                throw new InvalidDataException(
                    "capture reference dependency owner cannot be isolated from its masked target");
            }
            var source = result[ownerChunk];
            var ownerTargets = dependencies
                .Where(dependency => dependency.OwnerOrdinal == owner)
                .Select(dependency => dependency.TargetOrdinal)
                .ToHashSet();
            var isolationRoot = owner;
            for (var ancestor = nodes[checked((int)owner)].ParentOrdinal;
                ancestor != 0;
                ancestor = nodes[checked((int)ancestor)].ParentOrdinal)
            {
                if (ownerTargets.Contains(ancestor))
                {
                    // A local JointInstance endpoint cannot also be appended as
                    // an isolated serializer dependency: masking that endpoint's
                    // children would mask the owner itself. Keep every ancestor
                    // endpoint inside the extracted component instead.
                    isolationRoot = ancestor;
                }
            }
            if (source.RootOrdinals.Contains(isolationRoot))
            {
                throw new InvalidDataException(
                    "capture reference dependency root cannot contain its masked target");
            }

            var sourceMembers = source.MemberOrdinals.ToHashSet();
            var sourceFrontier = source.FrontierOrdinals.ToHashSet();
            var extractedMembers = new HashSet<uint>();
            var extractedFrontier = new HashSet<uint>();
            var stack = new List<uint> { isolationRoot };
            while (stack.Count != 0)
            {
                var last = stack.Count - 1;
                var ordinal = stack[last];
                stack.RemoveAt(last);
                if (sourceFrontier.Contains(ordinal))
                {
                    extractedFrontier.Add(ordinal);
                    continue;
                }
                if (!sourceMembers.Contains(ordinal))
                {
                    throw new InvalidDataException(
                        "capture dependency owner subtree crosses an unattested page boundary");
                }
                extractedMembers.Add(ordinal);
                var childStart = childOffsets[checked((int)ordinal)];
                var childEnd = childStart + childCounts[checked((int)ordinal)];
                for (var child = childEnd - 1; child >= childStart; child--)
                {
                    stack.Add(children[child]);
                }
            }

            var isolatedMemberOrdinals = source.MemberOrdinals
                .Where(extractedMembers.Contains)
                .ToArray();
            var isolatedFrontierOrdinals = source.FrontierOrdinals
                .Where(extractedFrontier.Contains)
                .ToArray();
            if (isolatedMemberOrdinals.Length == 0
                || isolatedMemberOrdinals[0] != isolationRoot)
            {
                throw new InvalidDataException(
                    "capture dependency owner isolation lost its component root");
            }

            var remainingFrontier = new List<uint>(
                source.FrontierOrdinals.Length - isolatedFrontierOrdinals.Length + 1);
            var insertedOwner = false;
            foreach (var ordinal in source.FrontierOrdinals)
            {
                if (extractedFrontier.Contains(ordinal))
                {
                    if (!insertedOwner)
                    {
                        remainingFrontier.Add(isolationRoot);
                        insertedOwner = true;
                    }
                    continue;
                }
                remainingFrontier.Add(ordinal);
            }
            if (!insertedOwner)
            {
                remainingFrontier.Add(isolationRoot);
            }

            var remainingMembers = source.MemberOrdinals
                .Where(ordinal => !extractedMembers.Contains(ordinal))
                .ToArray();
            if (remainingMembers.Length == 0)
            {
                throw new InvalidDataException(
                    "capture dependency owner isolation emptied its source chunk");
            }
            result[ownerChunk] = source with
            {
                FrontierOrdinals = remainingFrontier.ToArray(),
                MemberOrdinals = remainingMembers,
                NodeCount = checked((uint)remainingMembers.Length),
            };
            // Append extracted components after the original planner waves so
            // direct service roots remain the serialized-root prefix attested
            // by the capture envelope.
            result.Add(new(
                [isolationRoot],
                isolatedFrontierOrdinals,
                isolatedMemberOrdinals,
                [],
                checked((uint)isolatedMemberOrdinals.Length)));
        }
    }
}
