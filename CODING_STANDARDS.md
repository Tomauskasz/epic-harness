# Coding Standards

## Scope And Precedence

This is a general baseline for hand-authored production code and tests. A repository policy that explicitly governs the same concern takes precedence. Enforced tooling still applies, and this document does not arbitrate conflicts between repository policies.

The sections have separate jobs. The normative standards govern decisions. Brooks's lens classifies system-level decay. Fowler's catalogue flags local symptoms. A diagnostic is evidence to investigate, never a verdict by itself.

## Normative Standards

### Evidence And Contracts

- Before changing behavior, read the relevant source, consumers, dependencies, domain context, decisions, invariants, errors, lifecycle, configuration, and tests across the affected interface.
- Preserve required behavior and contracts unless the requested change explicitly replaces them.
- Derive values from real inputs and authoritative contracts. Do not invent defaults for missing product, security, ownership, or external-contract decisions.
- Use the smallest complete correction within the requested scope.
- Surface failure from every required operation. An undocumented fallback, skipped step, degraded result, or cosmetic success state is not success.
- Discover validators from maintained project sources, then run the nearest boundary capable of falsifying the claim. Record the exact command and evidence; discovery is not execution, unexpected mutation is failure, and missing validation remains unproven.
- Claim root cause only when the reported symptom fails before the correction and passes after it.
- Rank risk by demonstrated consequence, likelihood, and blast radius. Keep confidence separate from severity; effort, file size, and convenience do not lower impact.

### Knowledge And Domain

- Give each decision, invariant, constant, hierarchy, and algorithm one authoritative owner. Make projections, caches, generated output, and replicas traceable to it.
- Align names, behavior, and boundaries with maintained problem language and architectural decisions. Treat stale contracts and conflicts with recorded decisions as defects.

### Interfaces And Modules

- An interface is everything a caller must know: operations, types, invariants, ordering, errors, configuration, and performance expectations. Keep that knowledge smaller than the useful complexity it hides.
- A deep module concentrates a coherent responsibility behind a small interface. Deleting it should redistribute essential complexity; deleting a pass-through should remove only indirection.
- A seam permits one behavior to be substituted without caller edits. An adapter implements that seam; it is not the seam itself.

### Change Locality

- Assign each concern's behavior, bugs, and verification to one natural owner. Cross-layer contracts may coordinate owners without erasing their responsibilities.

### Complexity Budget

- Before adding an option, strategy, flag, helper, or extension point, test deletion, inlining, shrinking, and an existing native mechanism. Retain machinery only for demonstrated current variation or contract value.
- Every branch, state transition, type escape, indirection, and orchestration step must express required domain or contract complexity.

### Dependency Boundaries

- Keep dependency direction aligned with stable ownership. Direction inversions, cycles, unstable fan-out, fat interfaces, and conceptual inconsistency require correction when they make change or behavior unpredictable.
- Use team structure to explain architecture only when ownership evidence supports it.
- Keep in-process behavior direct and local test substitutes internal. Put an owned remote service behind a deployment port while retaining domain policy inside; put a third party behind a narrow behavior-specific port.
- Do not create a production-only adapter. Require a justified alternate implementation or test substitute, and never expose implementation details solely as public test seams.

### Operational Mechanics

- An operational mechanic is provider or infrastructure workflow with no domain-policy meaning. Extract it only when at least two callers share the mechanic and reason to change, a small explicit interface is possible, and the move improves locality or testability.
- A shared mechanic accepts explicit capabilities, returns structured results and errors, and hides provider and lifecycle details. Authorization, state transitions, timing, error meaning, and orchestration order stay with the policy owner.
- Extract one mechanic at a time: protect behavior, define the smallest interface, migrate one representative caller, verify success and failure paths, migrate the rest, then delete the duplicate implementations.

### Physical Topology

- Treat line count, file count, directory breadth, and related-file clusters as inspection signals only. Cohesion, responsibility, interface depth, navigation, testability, and blast radius decide structure.
- Split mixed responsibilities in behavior-preserving steps, even when the file is small. Keep a large cohesive unit; merge thin shards that implement one feature or interface; retain small modules with independent contracts.
- Give a genuine related-file cluster the smallest project-native package home and a narrow entry interface. Once that home exists, inspect same-domain siblings stranded elsewhere.
- Resolve a displaced facade by moving its behavior, preserving a documented contract, introducing a tiny compatibility shim, or running a tested deprecation. More repositories or hierarchy do not repair incoherent ownership.

### Test Architecture

- Cover the material unit, integration, end-to-end, and external-contract boundaries of the system. Make any intentional gap explicit.
- Observe behavior through stable interfaces. Retain narrower tests only for distinct diagnostic value, and remove obsolete shallow-seam tests after deeper behavior is protected.
- Characterize underspecified behavior at its exact boundary before a risky change.
- Repair suites whose brittleness, obscurity, duplication, mock abuse, coverage illusion, stale compatibility assertions, or layer mismatch prevents reliable change detection.

### Cleanup And Evolution

- Before deletion, inspect source, tests, configuration, documentation, exports, entrypoints, plugins, reflection, runtime discovery, downstream contracts, owners, and migration windows.
- Repository silence is not proof of disuse. Validate the deletion as an independent behavior change after it is made.
- Intentional debt must state the accepted tradeoff, accountable owner, and concrete payback condition.

### Change Attribution

- Make regression claims against a reliable base and distinguish introduced, worsened, and merely touched conditions. Include adjacent debt only when the change introduced it, worsened it, or now depends on it.
- Churn, timestamps, blame, and recent-commit counts do not establish causality. Exclude style-only observations from behavioral claims.

## Brooks's System Lens

- **Cognitive Overload:** classify system-wide maintainer working-set burden; raw code volume is insufficient.
- **Change Propagation:** classify synchronized migration or release coordination across deployment units or teams; source-level scattering belongs to the Fowler lens.
- **Knowledge Duplication:** classify competing truth across independently deployed services or authoritative artifacts; repeated executable implementation belongs to the Fowler lens.
- **Accidental Complexity:** classify nonessential system machinery; inherent domain complexity is excluded.
- **Dependency Disorder:** classify ownership obscured by the architectural dependency graph; dependency count alone is insufficient.
- **Domain Model Distortion:** classify maintained problem concepts changing meaning across system boundaries; personal naming preference is insufficient.

## Fowler's Code-Smell Catalogue

- **Mysterious Name** - **Signal:** an intended reader cannot infer the code or domain role. **Not sufficient:** unfamiliar but established local terminology.
- **Duplicated Code** - **Signal:** the same nontrivial executable implementation recurs in places that can diverge. **Not sufficient:** incidental syntax or repeated classification dispatch, which belongs to Repeated Switches.
- **Long Function** - **Signal:** one function owns too many sequential responsibilities or hidden phases. **Not sufficient:** length alone, generated code, a coherent linear transaction, a measured performance constraint, or complexity isolated to traversal, which belongs to Loops.
- **Long Parameter List** - **Signal:** callers must coordinate too many independent inputs or ordering rules. **Not sufficient:** a stable external, framework, or boundary signature; never bundle unrelated values merely to shorten it.
- **Global Data** - **Signal:** process-wide state is accessed without an explicit dependency owner, creating action at a distance. **Not sufficient:** immutable constants or deliberately controlled process configuration.
- **Mutable Data** - **Signal:** non-global mutation escapes its owner through aliases or temporal coupling. **Not sufficient:** process-wide state, private single-owner state, or an explicit state machine.
- **Divergent Change** - **Signal:** history shows one module changing for unrelated reasons. **Not sufficient:** one broad change.
- **Shotgun Surgery** - **Signal:** one concept forces edits across unrelated owners. **Not sufficient:** an intentional cross-layer contract change.
- **Feature Envy** - **Signal:** behavior primarily manipulates another owner's internal data or invariants. **Not sufficient:** legitimate coordination or querying.
- **Data Clumps** - **Signal:** recurring values travel together and collectively form a stable invariant. **Not sufficient:** incidental co-occurrence.
- **Primitive Obsession** - **Signal:** scalar values repeatedly encode domain rules or permit invalid states. **Not sufficient:** serialization or boundary values.
- **Repeated Switches** - **Signal:** classification over one axis is dispatched independently in places that can diverge. **Not sufficient:** one localized exhaustive match.
- **Loops** - **Signal:** iteration ordering or mutation obscures the invariant even when traversal is one responsibility. **Not sufficient:** a short clear traversal; a pipeline rewrite must improve correctness or clarity.
- **Lazy Element** - **Signal:** an element is empty or near-empty and owns no lifecycle, contract, or variation. **Not sufficient:** compatibility, public extension, discovery, wiring, predicted variation, or a forwarding role.
- **Speculative Generality** - **Signal:** unsupported future variation imposes present complexity. **Not sufficient:** a maintained extension contract with a current consumer.
- **Temporary Field** - **Signal:** a field is meaningful only during an implicit phase, leaving other object states ambiguous. **Not sufficient:** a documented lifecycle field in an explicit state machine.
- **Message Chains** - **Signal:** callers encode unstable navigation through an object graph. **Not sufficient:** fluent APIs, builders, or pipelines.
- **Middle Man** - **Signal:** one-to-one delegation gives callers no simpler or safer contract than the underlying collaborator. **Not sufficient:** compatibility, security, observability, or anti-corruption value.
- **Insider Trading** - **Signal:** modules exchange private knowledge or reach bidirectionally into each other's internals. **Not sufficient:** deliberate collaboration around a jointly owned encapsulated invariant.
- **Large Class** - **Signal:** a class owns unrelated responsibilities, interfaces, or collaborator sets. **Not sufficient:** size alone, or a cohesive facade, adapter, table, or framework boundary.
- **Alternative Classes With Different Interfaces** - **Signal:** genuinely substitutable roles expose needlessly different operations. **Not sufficient:** intentionally distinct domain contracts.
- **Data Class** - **Signal:** an internal domain object exposes data while its behavior systematically lives elsewhere. **Not sufficient:** a DTO, record, serialization type, or boundary snapshot.
- **Refused Bequest** - **Signal:** a subtype cannot honor inherited operations or invariants. **Not sufficient:** selective framework overrides.
- **Comments** - **Signal:** prose compensates for code that does not express what it does, or has become misleading. **Not sufficient:** rationale, legal text, generated markers, API contracts, or compatibility notes.
