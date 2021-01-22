package metadata

components: transforms: filter: {
	title: "Filter"

	description: """
		Filters events based on a set of conditions.
		"""

	vrl_replacement: {
		description: "You can now specify filtering conditions using VRL instead of the now-deprecated `check_fields`."
		examples: [
			#".hostname == "acmecorp.org" || .severity == "crit""#,
			".status_code < 300 && .status_code >= 200",
			#"includes(.stacktrace, "unauthorized operation")"#,
		]
	}

	classes: {
		commonly_used: true
		development:   "stable"
		egress_method: "stream"
	}

	features: {
		filter: {}
	}

	support: {
		targets: {
			"aarch64-unknown-linux-gnu":      true
			"aarch64-unknown-linux-musl":     true
			"armv7-unknown-linux-gnueabihf":  true
			"armv7-unknown-linux-musleabihf": true
			"x86_64-apple-darwin":            true
			"x86_64-pc-windows-msv":          true
			"x86_64-unknown-linux-gnu":       true
			"x86_64-unknown-linux-musl":      true
		}
		requirements: []
		warnings: []
		notices: []
	}

	configuration: {
		condition: {
			description: "The condition to be matched against every input event. Only messages that pass the condition will be forwarded."
			required:    true
			warnings: []
			type: object: configuration._conditions
		}
	}

	input: {
		logs: true
		metrics: {
			counter:      true
			distribution: true
			gauge:        true
			histogram:    true
			set:          true
			summary:      true
		}
	}

	examples: [
		{
			title: "Drop debug logs"
			configuration: {
				condition: {
					type:   "remap"
					source: '.level == "debug"'
				}
			}
			input: [
				{log: {
					level:   "debug"
					message: "I'm a noisy debug log"
				}},
				{log: {
					level:   "info"
					message: "I'm a normal info log"
				}},
			]
			output: [
				{log: {
					level:   "info"
					message: "I'm a normal info log"
				}},
			]
		},
	]

	telemetry: metrics: {
		events_discarded_total: components.sources.internal_metrics.output.metrics.events_discarded_total
	}
}
