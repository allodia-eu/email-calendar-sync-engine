require ["date","relational","vacation"];
# rule:[Out-of-Office]
if allof (currentdate :zone "+0100" :value "ge" "date" "2030-01-02", currentdate :zone "+0100" :value "le" "date" "2030-01-09")
{
	vacation "Away.";
}
