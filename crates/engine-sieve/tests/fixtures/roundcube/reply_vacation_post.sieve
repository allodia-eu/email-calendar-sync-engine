require ["date","relational","vacation"];
# rule:[Out of Office]
if allof (currentdate :zone "+0100" :value "ge" "iso8601" "2030-01-02T10:00:00+01:00", currentdate :zone "+0100" :value "le" "iso8601" "2030-01-09T18:30:00+01:00")
{
	vacation :days 7 :subject "Away" text:
I am away.
Back on the ninth.
.
;
}
