```text
NISSE — INVESTIGATION REVIEW
============================

PURPOSE
-------

Add a new review operation:

    review_investigation(task, result)

`task` describes what was supposed to be investigated.

`result` is the investigation output to review.

Both arguments may contain either literal text or a file reference.

A leading `@` explicitly means file.

Without `@`, an existing file path is treated as a file; otherwise the value
is treated as literal text.


ROLE
----

Nisse remains a reviewer.

The main agent or human performs the investigation.

Nisse evaluates whether the resulting claims and conclusions hold up.

This must work for investigations involving, for example:

    software
    source code
    operating systems
    embedded systems
    hardware
    protocols
    technical documentation
    regulations or other evidence-based subjects


REVIEW MODEL
------------

The investigation result is untrusted input.

Nisse must not inherit the investigator's claimed confidence, importance or
causal conclusions.

The review has two complementary parts.


CLAIM REVIEW

Examine the important claims and conclusions in `result`.

Determine whether each is:

    supported
    partially supported
    contradicted
    insufficiently supported
    not applicable
    outside scope

Pay particular attention to:

    evidence actually supporting the claim
    applicability to the target system/version/context
    unsupported assumptions or causal steps
    possibilities presented as conclusions
    overstated confidence or importance
    contradictions with known facts
    relevance to the original task

Nisse should distinguish between:

    demonstrated
    strongly supported
    plausible
    merely possible
    unsupported


COVERAGE REVIEW

Independently compare the investigation against `task`.

Identify important areas that the investigation should reasonably have
examined but missed or materially under-investigated.

A coverage gap means that the investigation appears incomplete.

It does not by itself constitute a newly proven finding.


INDEPENDENCE
------------

The same model may have produced the investigation and perform the review.

Reviewers must therefore run in fresh isolated sessions without the original
investigation conversation.

Claim review and coverage review should also be separate review tasks.

The claim reviewer should approach supplied findings skeptically and attempt
to falsify them rather than defend them.


EVIDENCE
--------

Reuse Nisse's existing retrieval, evidence and reviewer architecture.

Evidence may come from whatever sources are relevant to the task, such as:

    repository/source snapshots
    project files
    project knowledge
    specifications
    hardware documentation
    authoritative external sources

Nisse may perform bounded evidence lookup needed to verify a claim or a
suspected coverage gap.

The review must remain evidence-driven rather than becoming a new open-ended
research task.


DOMAIN HANDLING
---------------

Reuse Nisse's normal reviewer routing.

Domain-specific reviewers or `dynamic-domain-expert` may be selected as
appropriate.

The investigation-review mechanism itself must remain domain-independent.


OUTPUT
------

The result should clearly show:

    supported findings
    partially supported findings
    contradicted findings
    insufficiently supported findings
    non-applicable or out-of-scope findings
    overstated conclusions
    important coverage gaps

For material criticism, preserve enough evidence and reasoning to understand
why the original conclusion does not fully hold.


PI INTEGRATION
--------------

Expose:

    nisse_review_investigation(task, result)

Main Pi supplies the effective task, including relevant constraints and
clarifications from the investigation.

Nisse receives explicit input and does not implicitly consume Main Pi's
conversation history.


RELATION TO NISSE
-----------------

Nisse's primary review operations become:

    review_plan
    review_code
    review_investigation

They answer:

    review_plan
        Is this a sound plan?

    review_code
        Is this implementation sound?

    review_investigation
        Do these investigation results actually hold up?


Nisse remains a review system.

The investigator finds answers.

Nisse tests whether those answers survive scrutiny.
```
