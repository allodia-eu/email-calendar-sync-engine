require ["copy","vacation"];
# rule:[Out-of-Office]
if true
{
	vacation "Away.";
	redirect :copy "deputy@example.com";
}
