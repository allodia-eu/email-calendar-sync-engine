require ["body","fileinto","reject"];
# rule:[Quote "and" back\slash]
if header :contains "subject" "say \"hi\" \\ there"
{
	fileinto "Odd \"folder\"";
}
# rule:[Multi line]
if body :text :contains text:
first line
..second line
.

{
	reject text:
Not here.
Go away.
.
;
}
# rule:[Unicode é]
if header :contains "subject" "Größe"
{
	fileinto "Ärger";
}
