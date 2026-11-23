require ["date","relational","vacation"];
# rule:[Out-of-Office]
if false # allof (currentdate :zone "+0000" :value "ge" "iso8601" "2030-01-02T09:00:00Z", currentdate :zone "+0000" :value "le" "iso8601" "2030-01-09T17:30:00Z")
{
	vacation :subject "Away" "Away.";
}
