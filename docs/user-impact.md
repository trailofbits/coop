# User impact in feature and security work

Security and robustness are coop's primary goals. A control that users cannot
adopt or operate reliably may also fail to protect them. Assess user impact
when implementing and reviewing a feature, hardening measure, or changed
workflow.

## Assess the task

Describe what the user does today and after the change. Check the normal path,
failure and recovery, and both VM backends where relevant. Look for added
steps or prompts, setup requirements, delay, compatibility changes, surprising
denials, error messages, and loss of work or recovery. Ask whether users can
complete the task, how much effort it takes, and whether they can understand
and recover from a failure. Use observations or measurements when available;
state uncertainty rather than treating a predicted cost as an observed one.

For a security or reliability change, state the protected boundary or failure
mode and the expected benefit. Compare feasible alternatives that meet the
same objective with less user impact, including using an existing mechanism or
making the secure path easier to use. Record the meaningful advantages and
costs of each. Do not weaken or abandon a planned control solely because it
may add friction.

## Involve the requester in material trade-offs

A credible, material downside on a normal user path needs explicit acceptance
from the person who requested the work. Examples include a routine new prompt,
a broken existing workflow, substantial delay, or lost recovery. Present the
benefit, the affected task, evidence or uncertainty, feasible alternatives, and a
recommendation. Continue reversible work that makes the options concrete, but
do not silently settle the product trade-off. A minor or speculative cost can
be assessed without interrupting the workflow.

Negative user impact is not an automatic reason to reject a security or
hardening feature. The requester may choose the secure approach after weighing
its cost. Record the decision and explain how users adapt or recover. The
separate security confirmation triggers in [the trust model](trust-model.md)
still apply; accepting a user cost does not authorize weakening a trust
boundary.

## Review and PRs

Review the actual command and failure paths, not only the author's impact
claim. Flag an unexamined material cost or an unsupported claim with concrete
evidence and a feasible alternative when one exists. Report a genuine choice
as a product decision for the requester, not as an automatic code defect. A
trade-off already accepted with accurate documentation need not be raised
again unless the implementation changes it.

For a user-visible PR, describe the resulting workflow and any material cost,
the security or reliability benefit, the decision made or still needed, and
the evidence that supports the choice. Keep the description factual about the
code that exists.
