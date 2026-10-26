# {{ v.id }} {{ v.title }}

`{{ v.code }}`, {{ v.status }} {{ v.reason }}, {{ v.severity }}. Sent by the devlanes {{ v.version }} proxy, not by the service.

{{ v.summary }}
{% if !v.hint.is_empty() %}
{{ v.hint }}
{% endif -%}
{% if let Some(suggestion) = v.suggestion %}
Did you mean `{{ suggestion.host }}`{% if let Some(owner) = suggestion.owner %} ({{ owner }}){% endif %}?
{% endif -%}
{% if !v.logs.is_empty() %}
Recent output:

{{ fence }}
{{ v.logs }}
{{ fence }}
{% endif -%}
{% if !v.known.is_empty() %}
## Routed hostnames

{% for known in v.known -%}
- `{{ known.host }}`{% if let Some(owner) = known.owner %} ({{ owner }}){% endif %}
{% endfor -%}
{% if v.hidden > 0 %}- {{ v.hidden }} more in `devlanes url ls`
{% endif -%}
{% endif %}
## Diagnose
{% if v.remote %}
Run these on the box that runs the lane.
{% endif %}
{% for check in v.checks -%}
- {{ check.text }}{% if let Some(command) = check.command %}: `{{ command }}`{% endif %}
{% endfor %}
- Docs: {{ v.docs_url }}
- Open an issue: {{ v.issue_url }}
