# AI Policy

Use of agentic AI coding tools is permitted in this repository, subject to some firm rules that aim to ensure these
powerful tools are used competently and effectively by qualified developers.

All contributions to this repository, be it issues, commits, PRs, or comments, must be made by humans, and suitable for
human consumption. Any submission of AI slop outside of the narrow exceptions stated in this policy is grounds for
a permanent ban from contributing to this repository. Determination of what constitutes AI slop is at the sole
discretion of the repository maintainers.

If any generative AI technology is used to author any of the code in a pull request, that fact must be disclosed in the
PR description. Such code will be subject to higher scrutiny, and may be rejected if it is not up to the standards of
the repository maintainers.

Any code submitted to this repository must be reviewed by a human prior to submitting the pull request. Since even SOTA
agentic coding harnesses produce sloppy comments, over-complicated solutions, pointless tests, stupid logic, and various
other slop artifacts, it is the responsibility of the human in whose name the PR is submitted to filter out slop
artifacts after using agentic coding tools.

The above notwithstanding, it's often very useful to capture the output of an LLM as part of an issue description,
comment, or even a PR description. This is permitted, however any LLM-generated text must be wrapped in
a `<details></details>` block, with a `<summary>` tag set to "AI Slop: " followed by not more than 15 words summarizing
what is in the block (the summary can also be AI-generated). This allows humans reading the content to immediately see
what part is AI-generated, and treat it accordingly. Any such block must be preceded by human-authored text
establishing the context for the AI slop that follows. AI slop without any human-authored text preceding it, even if
it's disclosed, is completely unacceptable.

Commit messages must be human-authored, and follow our commit message standards. Clankers may not be used to compose
commit messages, although AI slop may be appended to a commit if it provides useful context provided it is delimited
thusly:

```
AI Slop: (summary goes here)
==
(slop text goes here)
```

The presence of clanker co-author tags in commit messages is absolutely not permitted. If you cannot be bothered to at
least remove your clanker's graffiti from your commit messages, you should not be contributing to this repository at all.

Agentic coding tools must not be used to produce code that you yourself are not able to understand or explain. If you
do not have the requisite skills and experience to operate in this codebase without machine augmentation, the use of
a clanker does not imbue you with those abilities.

Markdown files and other text are also subject to this same policy, with the exception of `AGENTS.md`, which being for
clankers to read is reasonable to author using a clanker. This file is explicitly exempted from the prohibition on AI
slop, and in fact probably is more effective if it has the insufferable on-distribution voice and Unicode-laden ticks of
modern LLMs.

anelson's thinking on LLMs and agentic coding tools more generally is heavily influenced by
[Oxide Computer's RFD 0576](https://rfd.shared.oxide.computer/rfd/0576), which should be considered applicable to this repository as well
unless contradicted by anything above.

This policy was inspired by [ripgrep's AI policy](https://github.com/BurntSushi/ripgrep/blob/master/AI_POLICY.md).
