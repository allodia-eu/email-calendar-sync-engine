require ["copy","fileinto"];
# rule:[Board]
if false # address :is "from" "board@example.com"
{
	fileinto :copy "Board";
}
# rule:[Newsletters]
if false # allof (header :contains "list-id" "news", not address :domain :is "from" "example.net")
{
	fileinto "Trash";
}
