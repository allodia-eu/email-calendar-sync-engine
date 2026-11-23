require ["date","fileinto","relational","vacation"];
# rule:[Out of office]
if allof (currentdate :value "ge" "date" "2030-01-02", currentdate :value "le" "date" "2030-01-09")
{
	vacation :days 1 :subject "Away" :from "carol@example.com" text:
I am away.
Back on the ninth.
.
;
}
# rule:[Invoices]
if header :contains "subject" "Invoice"
{
	fileinto "Finance";
}
