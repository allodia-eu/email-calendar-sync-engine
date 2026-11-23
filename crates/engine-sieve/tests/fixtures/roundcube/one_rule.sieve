require ["fileinto"];
# rule:[Invoices]
if header :contains "subject" "Invoice"
{
	fileinto "Finance";
	stop;
}
