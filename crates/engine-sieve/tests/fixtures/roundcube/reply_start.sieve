require ["date","relational","vacation"];
# rule:[Out-of-Office]
if currentdate :zone "+0000" :value "ge" "iso8601" "2030-01-02T09:00:00Z"
{
	vacation "Away from the second.";
}
