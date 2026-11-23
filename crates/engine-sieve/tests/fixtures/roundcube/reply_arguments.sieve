require ["vacation-seconds"];
# rule:[Out-of-Office]
if true
{
	vacation :seconds 3600 :addresses ["carol@example.com","c@example.com"] :subject "Away" :handle "away-1" :from "carol@example.com" "Away.";
}
