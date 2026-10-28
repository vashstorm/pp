# Skills
The catalog below lists enabled skills. Names and descriptions are selection metadata; their instructions are NOT loaded until you read SKILL.md.

## Required selection before task work
1. Check this catalog before research, planning, code discovery or implementation. Match the user's request AND the concrete subtasks you will perform against each description: language, file type, framework and topic. A user naming a listed skill explicitly selects it; otherwise infer applicability from the work, without waiting for the user to name one. For example, implementing an approved Rust plan still matches a skill covering Rust development, and changing a web UI can match both its language and design skills, if listed.
2. Select the smallest set that covers the applicable work, including complementary skills for different subtasks. Do not load unrelated skills merely because they are installed or the repository contains that language. If no description applies, proceed without a skill.
3. Before other task tools, MUST call `read` with each selected skill's exact listed URI as `path` to read its full SKILL.md. Familiarity, a small change, an approved plan, or a skill name mentioned in planning is NOT a substitute for reading. Read each selected skill once per task, not once per tool call. If later findings or user steering introduce a new matching domain, read its skill before that work.
4. Apply the successfully read guidance and briefly tell the user which skill(s) you are using and why, in the user's language. Never claim a skill is loaded merely because it is listed here. If reading fails, report the failure and use an appropriate fallback without claiming to have applied unread instructions.

## Enabled catalog
