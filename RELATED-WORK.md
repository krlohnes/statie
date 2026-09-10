# Related work

Literature behind two of statie's design questions: how to shape a
model's thinking (the `think` tool and the structured-thinking patterns
parked in statie-spec.md section 15), and how to run an agent on a
bounded, mutable state instead of an append-only transcript (the prime
directive: manage limited context through mutable state to get work
done). Each entry gives the finding as reported and what it means here.
Our own measurements referred to below are in README.md's field notes
(builds 1 to 7 of the bench battery) and the manual runs recorded in the
spec's "Thinking as a tool" section.

## 1. Structured thinking

### Self-correction and debate

- **Huang et al., "Large Language Models Cannot Self-Correct Reasoning
  Yet", ICLR 2024.** https://arxiv.org/abs/2310.01798
  Intrinsic self-correction, a model reviewing its own reasoning with no
  external signal, often makes answers worse. Models cannot locate their
  own reasoning errors but can fix them once the location is given.
  *For statie:* our rebuttal turn agreed with every position it was asked
  to attack. The judge must be a tool wherever a tool exists.
- **Du et al., "Improving Factuality and Reasoning in Language Models
  through Multiagent Debate", ICML 2024.** https://arxiv.org/abs/2305.14325
  Multiple instances proposing and debating over rounds improved math,
  strategy, and factual QA over single-agent baselines.
- **Smit et al., "Should we be going MAD? A Look at Multi-Agent Debate
  Strategies for LLMs", ICML 2024.** https://arxiv.org/abs/2311.17371
  Debate does not reliably beat self-consistency or ensembling; results
  are dominated by hyperparameters, chiefly how strongly agents are told
  to agree. *For statie:* one small model arguing both sides with the
  same evidence is the unsupported case; our debate judge picked a
  correct fix 0 of 6 times where the tool pick found one 4 of 6.
- **Khan et al., "Debating with More Persuasive LLMs Leads to More
  Truthful Answers", ICML 2024.** https://arxiv.org/abs/2402.06782
  Debate helped non-expert judges (76% vs 48%) when the debaters had
  evidence the judge did not. Gains come from asymmetric information,
  not from rhetoric.
- **"From Debate to Deliberation: Structured Collective Reasoning with
  Typed Epistemic Acts", 2026.** https://arxiv.org/abs/2603.11781
  Structured deliberation with typed moves beat free debate on
  non-routine decisions, produced decision packets with minority
  reports, cost about 62 times a single agent's tokens, and lost to a
  single model on routine decisions. *For statie:* the options-and-verdict
  pattern is for rare real forks only.

### Format and capacity

- **Tam et al., "Let Me Speak Freely? A Study on the Impact of Format
  Restrictions on Performance of Large Language Models", EMNLP 2024
  Industry.** https://arxiv.org/abs/2408.02442
  Constrained decoding (JSON mode) degrades reasoning; stricter
  constraints degrade more; looser formats improve accuracy and reduce
  variance on reasoning tasks.
- **Fan et al., "Capacity, Not Format: Rethinking Structured Reasoning
  Failures", 2026.** https://arxiv.org/abs/2606.09410
  The format cost depends on spare capacity: a strong model absorbs JSON
  (Sonnet 88.7% JSON vs 89.3% CoT on MATH-Hard), a model near its limit
  falls off a cliff (Haiku, 36.2 points, largely truncation). Remedy:
  think first, format later. *For statie:* this is the `think` tool's
  split exactly: the reasoning call has no schema, the acting call has
  the schema and no thinking. Under a schema our native-thinking runs
  returned nothing half the time; without it, 8 of 8 completed.

### How much thinking, and when

- **Muennighoff et al., "s1: Simple test-time scaling", EMNLP 2025.**
  https://arxiv.org/abs/2501.19393
  Budget forcing: append "Wait" when the model tries to stop, making it
  re-check and often fix a step. *For statie:* the "wait" is the moment
  that found the tax bug in our trace runs (2 of 5 fired it in cramped
  fields, 6 of 8 with a free field, 8 of 8 with the native channel).
- **Ghosal et al., "Does Thinking More Always Help? Mirage of Test-Time
  Scaling in Reasoning Models", NeurIPS 2025.**
  https://arxiv.org/abs/2506.04210
  Accuracy rises then falls with trace length; longer traces raise
  variance. Several short independent samples with a vote beat one long
  trace by up to 20% at the same budget. *For statie:* the candidate
  generator should sample k short thinking-off answers and let cargo
  pick, rather than one long call producing k candidates.
- **Cuadron et al., "The Danger of Overthinking: Examining the
  Reasoning-Action Dilemma in Agentic Tasks", 2025.**
  https://arxiv.org/abs/2502.08235
  On SWE-bench Verified, reasoning models favour their internal model
  over acting: analysis paralysis, rogue actions, premature quitting.
  Picking the lower-overthinking solution improved results almost 30% at
  43% less cost. *For statie:* the thought and the hands must be
  separate calls; our scratch-field runs got the diagnosis right 6 of 8
  and the edit right 2 of 8.
- **Liu et al., "Mind Your Step (by Step): Chain-of-Thought can Reduce
  Performance on Tasks where Thinking Makes Humans Worse", ICML 2025.**
  https://arxiv.org/abs/2410.21333
  Chain-of-thought lowers accuracy on some task classes, up to 36 points.
  *For statie:* "think everywhere" is wrong on principle; the trigger
  fires on a failure shape, not on every step.
- **Aghajohari et al., "The Markovian Thinker: Architecture-Agnostic
  Linear Scaling of Reasoning", ICLR 2026.**
  https://arxiv.org/abs/2510.06557
  Think in fixed 8k chunks; at each boundary reset to the question plus a
  short carry-over. Linear compute, constant memory, matches or beats
  long chain-of-thought at the same budget and extrapolates beyond it.
  *For statie:* a promptable bounded-window thinking pattern, and the
  direct answer to our 11k-token traces overrunning a 16k window.

### Breadth plus an external check

- **Wang et al., "Self-Consistency Improves Chain of Thought Reasoning
  in Language Models", 2022.** (ICLR 2023)
  Sample several reasoning paths, take the consistent answer.
- **Yao et al., "Tree of Thoughts: Deliberate Problem Solving with Large
  Language Models", NeurIPS 2023.**
  https://proceedings.neurips.cc/paper_files/paper/2023/file/271db9922b8d1f4dd7aaef84ed5ac703-Paper-Conference.pdf
  Generate, evaluate, expand, backtrack over partial solutions. Works to
  the extent the evaluator is good. *For statie:* the evaluator is the
  compiler where possible.
- **Xia et al., "Demystifying LLM-Based Software Engineering Agents"
  (Agentless), FSE 2025.** https://dl.acm.org/doi/abs/10.1145/3715754
  Sample several candidate patches, filter by regression tests and a
  generated reproduction test, pick by majority. Top open-source result
  on SWE-bench Lite at the time (32%) at a fraction of agent cost.
  *For statie:* the hypotheses-and-verify pattern, and the strongest
  evidence for it; our manual version was 5 of 5 on the trap fixture.
- **Shinn et al., "Reflexion: Language Agents with Verbal Reinforcement
  Learning", NeurIPS 2023.** https://arxiv.org/abs/2303.11366
  A short verbal note about a failure, kept across attempts, improves
  the next attempt. *For statie:* the `decision` field, and the case for
  recording losing candidates, not only the winner.

### Execution instead of imagined computation

- **Gao et al., "PAL: Program-aided Language Models", ICML 2023.**
  https://arxiv.org/abs/2211.10435 (and Chen et al., "Program of
  Thoughts", 2022.) Models decompose well and compute badly; hand the
  computation to an interpreter. *For statie:* our model computed
  `4600 / 100 * 90` as zero in every trace run, in the direction the
  smell pointed. The "actual" column must be measured, not imagined.
- **Chen et al., "Teaching Large Language Models to Self-Debug", ICLR
  2024.** https://arxiv.org/pdf/2304.05128
  Execution feedback beats the model's own dry-run trace as the signal
  for fixing code.
- **Zhong et al., "Debug like a Human: A Large Language Model Debugger
  via Verifying Runtime Execution Step by Step" (LDB), ACL 2024
  Findings.** https://arxiv.org/abs/2402.16906
  Split the program into basic blocks, run the failing test, show the
  variable values after each block, ask for a verdict per block. Up to
  9.8 points on code benchmarks. *For statie:* the shape for the
  trace-by-execution pattern: instrument, run, present values, ask which
  step first diverges.
- **"Empowering Autonomous Debugging Agents with Efficient Dynamic
  Analysis" (ADI), 2026.** https://arxiv.org/abs/2604.24212
  Function-level execution traces (a frame lifetime trace) instead of a
  line-by-line debugger. A basic agent reached 63.8% on SWE-bench
  Verified at $1.28 per task; 6.2 to 18.5 point gains when added to
  existing agents.

## 2. State mutation over long context

### Fixed-size state instead of a transcript (trained)

- **"IterResearch: Rethinking Long-Horizon Agents via Markovian State
  Reconstruction", 2025.** https://huggingface.co/papers/2511.07327
  Each step's context is rebuilt from the original question, an evolving
  model-written report, and the last interaction only. O(1) memory,
  trained with RL (efficiency-aware policy optimisation), scales to 2048
  interactions, 14.5 points over open-source agents. *For statie:* the
  closest published relative of the session loop: plan and
  `recent_summary` are the report, the working set is the last
  interaction. The difference is that their updater is trained.
- **Zhou et al., "MEM1: Learning to Synergize Memory and Reasoning for
  Efficient Long-Horizon Agents", ICLR 2026.**
  https://arxiv.org/abs/2506.15841
  A model trained to update one compact internal state per turn,
  consolidating memory and reasoning and discarding the rest. Near
  constant context; a 7B beat a 14B full-context model 3.5 times over
  with 3.7 times less memory, and generalised past the training horizon.
- **Yu et al., "MemAgent: Reshaping Long-Context LLM with Multi-Conv
  RL-based Memory Agent", 2025.** https://arxiv.org/abs/2507.02259
  Read a chunk, overwrite a fixed-length memory, repeat; answer from the
  final memory alone. A 32k-trained model held under 5% degradation to
  3.5 million tokens.
- **"Memory as Action: Autonomous Context Curation for Long-Horizon
  Agentic Tasks", 2025.** https://arxiv.org/pdf/2510.12635
  Context edits (deletion, insertion) as first-class actions in the
  policy, trained end to end. A 14B matched models 16 times larger with
  51% less context. *For statie:* the trained version of a patch-based
  workspace.
- **Common caveat.** All four train the state updater. The finding
  underneath them is that a prompted model does not manage its own
  state well, which is what our dogfood runs showed and why the harness
  became the author of state (`seen`, `notes`, `decision`).

### Prompting-only structured state

- **Rozanov and Rei, "StateAct: Enhancing LLM Base Agents via
  Self-prompting and State-tracking", 2024/2025.**
  https://arxiv.org/abs/2410.02810
  Restate the goal every turn, and emit a chain of states: small
  structured predictions about the environment (location, inventory).
  10 to 30% over ReAct across frontier models; composes with test-time
  scaling. *For statie:* `seen` and `decision` are chain-of-states
  written by tools rather than the model.
- **"LLM Agents Are Latent Context Managers: Eliciting Self-Managed
  Context via a Proprioceptive Dashboard" (VISTA), 2026.**
  https://arxiv.org/pdf/2606.30005
  A runtime dashboard of token usage, recency, archive status, and
  remaining budget, plus typed addressable context blocks the model
  keeps or archives (archived blocks recoverable in full). Gemini-3-Flash
  from 22.7 to 50.7% on a long-context agent benchmark; gains grow with
  context pressure and transfer across backbones. *For statie:* the
  harness view is a partial dashboard; keep-or-archive over typed blocks
  is the idea to take.
- **Lanchantin et al., "Learning to Reason and Memorize with
  Self-Notes", NeurIPS 2023.** https://arxiv.org/abs/2305.00833
  Interleave notes with the input rather than think only at the end;
  beat scratchpad and chain-of-thought.
- **Packer et al., "MemGPT: Towards LLMs as Operating Systems", 2023.**
  https://arxiv.org/abs/2310.08560
  Main context versus external context with explicit paging between
  them. *For statie:* `recall` over the session log is the page-in.
- **Zhang et al., "Recursive Language Models", 2025.**
  https://arxiv.org/abs/2512.24601
  Treat the context as a programmable variable the model inspects and
  recurses over, rather than data it must hold. Handled inputs two
  orders of magnitude beyond the window.

### Compaction and context rot

- **Anthropic, "Effective context engineering for AI agents", 2025.**
  https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
  Compaction, structured note-taking outside the window, and sub-agents
  returning compact summaries; tool-result clearing as the safest form
  of compaction.
- **Kang et al., "ACON: Optimizing Context Compression for Long-horizon
  LLM Agents", ICML 2026.** https://arxiv.org/abs/2510.00615
  Optimise the compression guideline in natural language from paired
  trajectories where full context succeeded and compressed context
  failed; distil the compressor. 26 to 54% less peak context with
  accuracy preserved; up to 46% improvement for small models as
  long-horizon agents. *For statie:* the rule audit and bench are this
  paired-failure loop done by hand.
- **Chroma, "Context Rot: How Increasing Input Tokens Impacts LLM
  Performance", 2025.** https://www.trychroma.com/research/context-rot
  18 frontier models degrade at every length increment, well before the
  window is full.
- **"Diagnosing and Mitigating Context Rot in Long-horizon Search",
  2026.** https://arxiv.org/pdf/2606.29718
  The failure is premature termination: with long contexts, models give
  up or answer wrongly long before running out of room, and the rate
  rises with length. Context management reduces it; behaviour-aware
  parallel sampling adds 2.6 to 4.9%. *For statie:* the `ask_user` park
  that grows with step count, named.
- **"Memory for Autonomous LLM Agents: Mechanisms, Evaluation, and
  Emerging Frontiers" (survey), 2026.** https://arxiv.org/abs/2603.07670
  Five families: context-resident compression, retrieval stores,
  reflective self-improvement, hierarchical virtual context,
  policy-learned management. Names the evaluation gap: benchmarks that
  interleave memory with decision-making rather than testing recall in
  isolation.

## 3. What this leaves open

- Nobody has measured harness-owned against model-owned state under a
  fixed budget with the model held constant. The bench battery is a
  small instrument for exactly that, and it is the prime directive's
  question.
- The trained fixed-state systems say the updater is the bottleneck.
  Without training, the levers are tools that write state, a separate
  maintainer call, and the harness deciding what survives.
- Structured-thinking order supported by the evidence: hypotheses and
  verify (Agentless, self-consistency), then trace by execution (LDB,
  PAL), then bounded-window thinking (Markovian Thinker), with
  options-and-verdict reserved for rare forks.
