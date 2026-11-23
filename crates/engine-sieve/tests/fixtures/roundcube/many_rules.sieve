require ["body","copy","fileinto","imap4flags","regex"];
# rule:[Any of two]
if anyof (header :contains "subject" ["urgent","asap"], address :domain :is ["to","cc"] "example.org")
{
	addflag "\\Flagged";
	addflag ["\\Seen","$label1"];
}
# rule:[All of two]
if allof (header :matches "subject" "report*", not header :is "x-spam-flag" "YES")
{
	redirect :copy "archive@example.com";
}
# rule:[Big]
if size :over 5M
{
	discard;
}
if body :text :contains "unsubscribe"
{
	fileinto "Lists";
	stop;
}
# rule:[Regex]
if header :regex "subject" "^\\[ticket-[0-9]+\\]"
{
	setflag "$ticket";
}
# rule:[Everything]
if true
{
	keep;
}
