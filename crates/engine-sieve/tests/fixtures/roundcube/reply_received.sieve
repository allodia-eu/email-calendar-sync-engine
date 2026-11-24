require ["regex","vacation"];
# rule:[Out-of-Office]
if anyof (header :regex "received" "(30|31) Jan 2030", header :regex "received" "([ 0]1|[ 0]2) Feb 2030")
{
	vacation :subject "Away" "Away.";
}
